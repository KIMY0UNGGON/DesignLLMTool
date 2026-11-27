use designllm_contracts::{new_id, Diagnostic, DltError, DltResult, ErrorCode, PDF_EDIT_FAILED, PDF_EDIT_NO_TEXT, PDF_EDIT_WORD_REQUIRED};
use designllm_runtimes::win::{self, JobObject};
use std::{fs, io::{BufRead, BufReader, Read, Write}, os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle}, path::{Path, PathBuf}, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};
use windows_sys::Win32::{
    Foundation::FILETIME,
    System::Threading::{GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
    UI::WindowsAndMessaging::GetWindowThreadProcessId,
};

const CONVERT: &str = r#"
On Error Resume Next
Set word = CreateObject("Word.Application")
If Err.Number <> 0 Then WScript.Quit 2
Set scratch = word.Documents.Add
If Err.Number <> 0 Then WScript.Quit 3
owner = scratch.ActiveWindow.Hwnd
If Err.Number <> 0 Then
    scratch.Close 0
    WScript.Quit 3
End If
WScript.StdOut.WriteLine owner
command = WScript.StdIn.ReadLine
If Err.Number <> 0 Or command <> "convert" Then
    scratch.Close 0
    WScript.Quit 3
End If
word.AutomationSecurity = 3
word.DisplayAlerts = 0
If Err.Number <> 0 Then
    word.Quit 0
    WScript.Quit 3
End If
scratch.Close 0
Set doc = word.Documents.OpenNoRepairDialog(WScript.Arguments(0), False, True, False)
If Err.Number <> 0 Then
    word.Quit 0
    WScript.Quit 3
End If
status = 0
body = Trim(Replace(Replace(Replace(doc.Content.Text, Chr(13), ""), Chr(7), ""), Chr(1), ""))
If Err.Number <> 0 Then
    status = 3
ElseIf Len(body) = 0 Then
    status = 4
Else
    Call doc.SaveAs2(WScript.Arguments(1), 12)
    If Err.Number <> 0 Then status = 3
End If
doc.Close 0
word.Quit 0
WScript.Quit status
"#;

struct ConversionFolder(PathBuf);

impl Drop for ConversionFolder {
    fn drop(&mut self) {
        for name in ["convert.vbs", "source.pdf", "converted.docx"] {
            let _ = fs::remove_file(self.0.join(name));
        }
        let _ = fs::remove_dir(&self.0);
    }
}

pub(crate) fn convert(pdf: &[u8], cancel: &designllm_core::PdfCancel) -> DltResult<Vec<u8>> {
    if cancel.is_canceled() { return Err(DltError::new(ErrorCode::Canceled, "PDF conversion was canceled")); }
    if !pdf.starts_with(b"%PDF-") { return Err(DltError::invalid("the file does not have a PDF header")); }
    let temp = std::env::temp_dir().join(format!("designllm-pdf-edit-{}", new_id()));
    fs::create_dir(&temp)?;
    let folder = ConversionFolder(temp);
    let script = folder.0.join("convert.vbs");
    let source = folder.0.join("source.pdf");
    let target = folder.0.join("converted.docx");
    fs::write(&script, CONVERT)?;
    fs::write(&source, pdf)?;

    let windows = std::env::var_os("SystemRoot").ok_or_else(|| DltError::unsupported("Windows Script Host is unavailable"))?;
    let cscript = PathBuf::from(windows).join("System32/cscript.exe");
    let args = ["//Nologo".to_owned(), "//B".to_owned(), "//T:65".to_owned(),
        script.to_string_lossy().into_owned(), source.to_string_lossy().into_owned(), target.to_string_lossy().into_owned()];
    let launched = SystemTime::now();
    let (mut process, job) = win::spawn_suspended_in_job(&cscript, &args, &folder.0)?;
    let mut stdin = process.stdin.take().expect("spawn provides stdin");
    let stdout = process.stdout.take().expect("spawn provides stdout");
    let (sender, owner) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        if BufReader::new(stdout).take(64).read_line(&mut line).is_ok() {
            let _ = sender.send(line.trim().parse::<isize>().ok());
        }
    });
    win::resume_child(&process)?;
    let deadline = Instant::now() + Duration::from_secs(70);
    let mut waiting_for_word = true;
    let mut owns_word = false;
    let mut cancel_deadline = None;
    let status = loop {
        if let Some(status) = process.try_wait()? { break status; }
        let now = Instant::now();
        let canceled = cancel.is_canceled();
        // If Word is still starting, give the script a short chance to identify it before ending the job.
        if canceled { cancel_deadline.get_or_insert(now + Duration::from_secs(1)); }
        if waiting_for_word {
            if let Ok(window) = owner.try_recv() {
                owns_word = window.is_some_and(|window| assign_word(&job, window, launched).is_ok());
                let command = if owns_word && !canceled && now < deadline { b"convert\n".as_slice() } else { b"cancel\n".as_slice() };
                stdin.write_all(command)?;
                waiting_for_word = false;
            }
        }
        if now >= deadline || (canceled && owns_word) || cancel_deadline.is_some_and(|at| now >= at) {
            let _ = job.terminate(1);
            let _ = process.wait();
            if canceled { return Err(DltError::new(ErrorCode::Canceled, "PDF conversion was canceled")); }
            return Err(conversion_error(ErrorCode::Unavailable, PDF_EDIT_FAILED, "PDF conversion did not finish in 70 seconds"));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    match status.code() {
        Some(0) if target.is_file() => {}
        Some(2) => return Err(conversion_error(ErrorCode::UnsupportedOperation, PDF_EDIT_WORD_REQUIRED, "editing PDFs requires Microsoft Word on this PC")),
        Some(4) => return Err(conversion_error(ErrorCode::UnsupportedOperation, PDF_EDIT_NO_TEXT, "PDF conversion found no editable text")),
        _ => return Err(conversion_error(ErrorCode::UnsupportedOperation, PDF_EDIT_FAILED, "Word could not convert the PDF into an editable document")),
    }
    let mut bytes = Vec::new();
    fs::File::open(target)?.take(designllm_core::MAX_IMPORT_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > designllm_core::MAX_IMPORT_BYTES {
        return Err(DltError::invalid("converted DOCX exceeds the import size limit"));
    }
    Ok(bytes)
}

fn assign_word(job: &JobObject, window: isize, launched: SystemTime) -> std::io::Result<()> {
    let invalid_owner = || std::io::Error::other("the converter did not identify a new Word process");
    let mut pid = 0;
    // Word owns this window until the script receives the proceed signal.
    if unsafe { GetWindowThreadProcessId(window as _, &mut pid) } == 0 { return Err(invalid_owner()); }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
    if process.is_null() { return Err(std::io::Error::last_os_error()); }
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    let handle = process.as_raw_handle();
    let (mut created, mut exited, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    let mut path = [0u16; 1024];
    let mut length = path.len() as u32;
    if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } == 0
        || unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut length) } == 0
    { return Err(std::io::Error::last_os_error()); }
    let created = u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime);
    let started = launched.duration_since(UNIX_EPOCH).map_err(std::io::Error::other)?.as_nanos() / 100 + 116_444_736_000_000_000;
    let path = String::from_utf16_lossy(&path[..length as usize]);
    if u128::from(created) < started || Path::new(&path).file_name().is_none_or(|name| !name.eq_ignore_ascii_case("winword.exe")) {
        return Err(invalid_owner());
    }
    job.assign(handle)
}

fn conversion_error(error_code: ErrorCode, diagnostic_code: &str, message: &str) -> DltError {
    let mut error = DltError::new(error_code, message);
    error.diagnostics.push(Diagnostic { code: diagnostic_code.into(), message: message.into(), node_id: None, blocking: true, params: Default::default() });
    error
}
