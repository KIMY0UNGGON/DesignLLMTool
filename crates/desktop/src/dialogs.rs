//! The Windows file dialogs (GUI_DESIGN §12, step 12): the system's own open and save dialogs, so
//! that the user never types a path. They are modal to the editor window and return `None` when
//! the user cancels or the dialog cannot be shown.

use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

/// Set when the app runs without a visible main window (`--smoke-test`, headless captures). Every native window the
/// app could open (file dialogs, the WebView2 preview) is refused then: a capture must never put a window on the
/// user's screen or wait for one.
static HEADLESS: AtomicBool = AtomicBool::new(false);

pub fn set_headless(on: bool) {
    HEADLESS.store(on, Ordering::SeqCst);
}

thread_local! {
    /// Task 69: the paths a UI-test run queued for the open dialogs (`ui.input` `dialog_answers`, already checked by the agent
    /// path rules); None outside such a run. The window opens its dialogs on its own thread.
    static TEST_ANSWERS: std::cell::RefCell<Option<std::collections::VecDeque<PathBuf>>> = const { std::cell::RefCell::new(None) };
    /// Task 68r: the frame being drawn carries a person's input (`ui_test_input::inject_ui_test_input`): a dialog it opens is
    /// the person's, and takes no queued answer.
    static PERSON_FRAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Task 68r: the files the save stand-in named, newest last (at most [`MAX_SAVE_ANSWERS`]). A save or an export to one of
    /// them is the agent's choice of place whenever it leaves (after a dry run, a loss a person accepts, a later plain save).
    static SAVE_ANSWERS: std::cell::RefCell<std::collections::VecDeque<String>> = const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
}

const MAX_SAVE_ANSWERS: usize = 256;

/// Installs (Some) or removes (None) the stand-in of a UI-test run; returns how many answers the removed queue still held.
pub fn set_test_answers(answers: Option<std::collections::VecDeque<PathBuf>>) -> usize {
    TEST_ANSWERS.with(|queue| std::mem::replace(&mut *queue.borrow_mut(), answers)).map_or(0, |left| left.len())
}

/// Whether the frame being drawn carries a person's input; set at the start of each frame of a UI-test window.
pub fn set_person_frame(on: bool) {
    PERSON_FRAME.with(|frame| frame.set(on));
}

/// Whether the save stand-in named `path` (compared as the window sends it, trimmed) in this window.
pub fn is_test_save_answer(path: &str) -> bool {
    let path = path.trim();
    SAVE_ANSWERS.with(|given| given.borrow().iter().any(|answer| answer == path))
}

/// The stand-in's answer while a UI-test run plays: Some(the next queued path, or None = cancelled when none is left).
/// None when no stand-in is installed, or the frame is a person's: the native dialog (or its headless refusal) answers. Open
/// and save dialogs share the queue; `check` is the rule of the dialog that takes the answer.
fn take_test_answer(check: fn(&str) -> Option<String>) -> Option<Option<PathBuf>> {
    if PERSON_FRAME.with(std::cell::Cell::get) {
        return None;
    }
    let answer = TEST_ANSWERS.with(|queue| queue.borrow_mut().as_mut().map(std::collections::VecDeque::pop_front))?;
    // Checked again when the dialog would return it (the run checked it when it started): a path changed since is cancelled.
    Some(answer.filter(|path| match check(&path.to_string_lossy()) {
        None => true,
        Some(reason) => {
            eprintln!("designllm-desktop: test dialog answer refused: {reason}");
            false
        }
    }))
}

fn test_answer() -> Option<Option<PathBuf>> {
    take_test_answer(designllm_contracts::local_path::agent_file_refusal)
}

/// Whether native windows are refused now: in a headless run, and always in tests.
pub fn headless() -> bool {
    cfg!(test) || HEADLESS.load(Ordering::SeqCst)
}

/// Whether the WebView2 preview window (`view.play`, web documents) must stay closed now.
pub fn preview_refused() -> bool {
    refused("web preview window")
}

/// The visible top-level windows that belong to this process (the tests' check that nothing opened a window).
#[cfg(test)]
pub fn visible_windows_of_this_process() -> usize {
    use windows::core::BOOL;
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, IsWindowVisible};
    unsafe extern "system" fn count(window: HWND, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the address of the counter below, alive for the whole enumeration.
        unsafe {
            let mut owner = 0u32;
            GetWindowThreadProcessId(window, Some(&mut owner));
            if owner == std::process::id() && IsWindowVisible(window).as_bool() {
                *(data.0 as *mut usize) += 1;
            }
        }
        BOOL(1)
    }
    let mut found = 0usize;
    // SAFETY: the callback only reads window facts and writes the counter it is given.
    unsafe {
        let _ = EnumWindows(Some(count), LPARAM(&mut found as *mut usize as isize));
    }
    found
}

/// Refuses a native window in a headless run, with a line in the log saying which one.
fn refused(what: &str) -> bool {
    if headless() {
        eprintln!("designllm-desktop: {what} not shown: no visible window (smoke or headless run)");
        return true;
    }
    false
}

use windows::{
    core::{HSTRING, PCWSTR},
    Win32::{
        Foundation::HWND,
        System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED},
        UI::Shell::{
            Common::COMDLG_FILTERSPEC,
            FileOpenDialog, FileSaveDialog, IFileOpenDialog, IFileSaveDialog, IShellItem,
            FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
        },
    },
};

/// The pictures an image node can take.
const IMAGE_FILTERS: &[(&str, &str)] = &[("이미지", "*.png;*.jpg;*.jpeg;*.gif;*.bmp;*.webp")];
/// The first D19 replacement slice preserves object identity for these raster formats only.
const IMAGE_REPLACEMENT_FILTERS: &[(&str, &str)] = &[("교체할 이미지", "*.png;*.jpg;*.jpeg;*.bmp")];
const VIDEO_FILTERS: &[(&str, &str)] = &[("동영상", "*.mp4;*.webm")];
/// The evidence a Figure's AI request can carry (`figure_ai_attach`): PDF, CSV/TSV and plain text.
const FIGURE_EVIDENCE_FILTERS: &[(&str, &str)] = &[("근거 자료", "*.pdf;*.csv;*.tsv;*.txt;*.md")];

/// The formats the editor opens (SPEC §6): documents, slides, figures' data and images.
const OPEN_FILTERS: &[(&str, &str)] = &[
    ("지원하는 파일", "*.designllm;*.docx;*.hwpx;*.hwp;*.pptx;*.pdf;*.csv;*.tsv;*.png;*.jpg;*.jpeg"),
    ("DesignLLMTool 작업물", "*.designllm"),
    ("문서", "*.docx;*.hwpx;*.hwp;*.pdf"),
    ("슬라이드", "*.pptx"),
    ("데이터", "*.csv;*.tsv"),
];

pub fn open_image(owner: Option<isize>) -> Option<PathBuf> {
    show_open(owner, IMAGE_FILTERS)
}

pub fn open_video(owner: Option<isize>) -> Option<PathBuf> {
    show_open(owner, VIDEO_FILTERS)
}

pub fn open_figure_evidence(owner: Option<isize>) -> Option<PathBuf> {
    show_open(owner, FIGURE_EVIDENCE_FILTERS)
}

pub fn open_image_replacement(owner: Option<isize>) -> Option<PathBuf> {
    show_open(owner, IMAGE_REPLACEMENT_FILTERS)
}

/// Shows the open dialog for one of the supported files. `owner` is the editor window handle.
pub fn open_document(owner: Option<isize>) -> Option<PathBuf> {
    show_open(owner, OPEN_FILTERS)
}

pub fn open_project_folder(owner: Option<isize>) -> Option<PathBuf> {
    if refused("project folder dialog") || TEST_ANSWERS.with(|queue| queue.borrow().is_some()) && !PERSON_FRAME.with(std::cell::Cell::get) {
        return None;
    }
    // The user chooses an existing folder in the shell; scripted file-dialog answers never launch other projects.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        dialog.SetOptions(dialog.GetOptions().ok()? | FOS_FORCEFILESYSTEM | FOS_PICKFOLDERS).ok()?;
        dialog.SetTitle(&HSTRING::from(&*crate::i18n::tr("프로젝트 폴더 열기"))).ok()?;
        dialog.Show(owner.map(|raw| HWND(raw as *mut core::ffi::c_void))).ok()?;
        path_of(&dialog.GetResult().ok()?)
    }
}

fn show_open(owner: Option<isize>, filters: &[(&str, &str)]) -> Option<PathBuf> {
    if let Some(answer) = test_answer() {
        return answer;
    }
    // A headless run (smoke, tests) never opens a window on the user's screen: the dialog counts as cancelled.
    if refused("open dialog") {
        return None;
    }
    // SAFETY: COM is used on the window's own thread; the dialog objects are dropped at return.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog.SetOptions(options | FOS_FORCEFILESYSTEM | FOS_FILEMUSTEXIST).ok()?;
        let names: Vec<(HSTRING, HSTRING)> = filters.iter().map(|(name, spec)| (HSTRING::from(&*crate::i18n::tr(name)), HSTRING::from(*spec))).collect();
        let specs: Vec<COMDLG_FILTERSPEC> = names
            .iter()
            .map(|(name, spec)| COMDLG_FILTERSPEC { pszName: PCWSTR(name.as_ptr()), pszSpec: PCWSTR(spec.as_ptr()) })
            .collect();
        dialog.SetFileTypes(&specs).ok()?;
        dialog.Show(owner.map(|raw| HWND(raw as *mut core::ffi::c_void))).ok()?;
        let item = dialog.GetResult().ok()?;
        path_of(&item)
    }
}

/// Shows the save dialog for the artifact's native format, suggesting its title.
pub fn save_document(owner: Option<isize>, title: String, kind: designllm_contracts::ArtifactKind) -> Option<PathBuf> {
    let name = if title.trim().is_empty() { crate::i18n::tr("새 작업물").into_owned() } else { title };
    let extension = designllm_contracts::artifact_native_extension(kind);
    show_save(owner, &name, &format!("*.{extension}"), extension)
}

/// Shows the save dialog for an export in `extension` (without the dot).
pub fn save_export(owner: Option<isize>, extension: &str) -> Option<PathBuf> {
    let spec = format!("*.{extension}");
    show_save(owner, &crate::i18n::tr("내보내기"), &spec, extension)
}

fn show_save(owner: Option<isize>, suggested: &str, spec: &str, label: &str) -> Option<PathBuf> {
    if let Some(answer) = take_test_answer(designllm_contracts::local_path::save_target_refusal) {
        if let Some(path) = answer.as_ref() {
            SAVE_ANSWERS.with(|given| {
                let mut given = given.borrow_mut();
                if given.len() == MAX_SAVE_ANSWERS {
                    given.pop_front();
                }
                given.push_back(path.to_string_lossy().trim().to_owned());
            });
        }
        return answer;
    }
    if refused("save dialog") {
        return None;
    }
    // SAFETY: as in `open_document`.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dialog: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog.SetOptions(options | FOS_FORCEFILESYSTEM).ok()?;
        let name = HSTRING::from(label);
        let pattern = HSTRING::from(spec);
        let filter = COMDLG_FILTERSPEC { pszName: PCWSTR(name.as_ptr()), pszSpec: PCWSTR(pattern.as_ptr()) };
        dialog.SetFileTypes(&[filter]).ok()?;
        if let Some(extension) = spec.strip_prefix("*.") {
            let extension = HSTRING::from(extension);
            dialog.SetDefaultExtension(PCWSTR(extension.as_ptr())).ok()?;
        }
        let file_name = HSTRING::from(suggested);
        dialog.SetFileName(PCWSTR(file_name.as_ptr())).ok()?;
        dialog.Show(owner.map(|raw| HWND(raw as *mut core::ffi::c_void))).ok()?;
        let item = dialog.GetResult().ok()?;
        path_of(&item)
    }
}

/// The file-system path of a shell item, when it has one.
///
/// # Safety
/// The caller must be on a thread where COM is initialised.
unsafe fn path_of(item: &IShellItem) -> Option<PathBuf> {
    let display = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let text = display.to_string().ok();
    CoTaskMemFree(Some(display.0 as *const core::ffi::c_void));
    text.map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_open_filter_names_every_format_the_editor_reads() {
        let all = OPEN_FILTERS[0].1;
        for extension in ["docx", "hwpx", "hwp", "pptx", "pdf", "csv", "tsv", "png"] {
            assert!(all.contains(extension), "{extension} is missing from the open filter");
        }
    }

    #[test]
    fn image_replacement_picker_only_lists_the_first_supported_raster_formats() {
        assert_eq!(IMAGE_REPLACEMENT_FILTERS[0].1, "*.png;*.jpg;*.jpeg;*.bmp");
        assert!(!IMAGE_REPLACEMENT_FILTERS[0].1.contains("gif"));
        assert!(!IMAGE_REPLACEMENT_FILTERS[0].1.contains("webp"));
        assert!(IMAGE_FILTERS[0].1.contains("gif") && IMAGE_FILTERS[0].1.contains("webp"), "adding a replacement picker does not narrow ordinary image insertion");
    }

    /// Task 69: outside a UI-test run the open dialog is the native one (refused in tests, as in a headless run); a run's
    /// stand-in answers in queue order, then counts as cancelled, and removing it reports what was not taken.
    #[test]
    fn the_test_stand_in_answers_only_while_installed() {
        assert_eq!(open_image(None), None, "no stand-in: the native dialog, refused in tests");
        let dir = std::env::temp_dir().join(format!("designllm-stand-in-{}", designllm_contracts::new_id()));
        std::fs::create_dir_all(&dir).expect("a folder");
        let [a, b, c] = ["a.png", "b.jpg", "c.gif"].map(|name| dir.join(name));
        for file in [&a, &b, &c] {
            std::fs::write(file, b"x").expect("a file");
        }
        let queue = [a.clone(), b.clone(), c].into_iter().collect();
        assert_eq!(set_test_answers(Some(queue)), 0);
        assert_eq!(open_image(None), Some(a));
        assert_eq!(open_video(None), Some(b.clone()), "the queue is shared by every open dialog");
        assert_eq!(set_test_answers(None), 1, "one answer was not taken");
        assert_eq!(open_image(None), None, "removed: the native path again");
        assert_eq!(set_test_answers(Some(Default::default())), 0);
        assert_eq!(open_image(None), None, "an empty queue cancels");
        // A queued file that is gone by the time the dialog would return it is not returned.
        std::fs::remove_file(&b).expect("removed");
        set_test_answers(Some([b].into_iter().collect()));
        assert_eq!(open_image(None), None, "checked again when it is taken");
        set_test_answers(None);
        std::fs::remove_dir_all(dir).expect("cleaned");
    }

    /// Task 68r: a save dialog takes the same queue, and its rule admits a new file in an existing folder only.
    #[test]
    fn the_save_stand_in_takes_a_new_file_in_an_existing_folder() {
        let dir = std::env::temp_dir().join(format!("designllm-save-answer-{}", designllm_contracts::new_id()));
        std::fs::create_dir_all(&dir).expect("a folder");
        let out = dir.join("out.png");
        assert_eq!(save_export(None, "png"), None, "no stand-in: the native dialog, refused in tests");
        set_test_answers(Some([out.clone()].into_iter().collect()));
        assert_eq!(save_export(None, "png"), Some(out.clone()), "a file that does not exist yet");
        assert_eq!(set_test_answers(None), 0);
        assert!(is_test_save_answer(&out.to_string_lossy()), "the place the stand-in named is remembered");
        // A dialog a person's frame opens is the native one: the queue keeps its answer.
        let person = dir.join("person.png");
        set_test_answers(Some([person.clone()].into_iter().collect()));
        set_person_frame(true);
        assert_eq!(save_document(None, String::new(), designllm_contracts::ArtifactKind::Docx), None, "the native dialog, refused in tests");
        set_person_frame(false);
        assert_eq!(set_test_answers(None), 1, "the person's dialog took no answer");
        assert!(!is_test_save_answer(&person.to_string_lossy()));
        let missing_folder = dir.join("no-such-folder").join("out.png");
        let directory = dir.join("a-folder");
        std::fs::create_dir_all(&directory).expect("a folder");
        let mut refused = vec![missing_folder, directory, PathBuf::from("out.png")];
        if cfg!(windows) {
            refused.push(dir.join("CON.png"));
            refused.push(PathBuf::from(r"\\server\share\out.png"));
        }
        for bad in refused {
            set_test_answers(Some([bad.clone()].into_iter().collect()));
            assert_eq!(save_export(None, "png"), None, "{}", bad.display());
            assert_eq!(set_test_answers(None), 0, "the save dialog did not take the answer: {}", bad.display());
        }
        std::fs::remove_dir_all(dir).expect("cleaned");
    }

    #[test]
    fn the_video_picker_only_lists_the_supported_containers() {
        assert_eq!(VIDEO_FILTERS[0].1, "*.mp4;*.webm");
    }

    #[test]
    fn the_evidence_picker_lists_the_pdf_table_and_text_formats_the_request_accepts() {
        for extension in ["pdf", "csv", "tsv", "txt", "md"] {
            assert!(FIGURE_EVIDENCE_FILTERS[0].1.contains(extension), "{extension} is missing from the evidence picker");
        }
        assert!(!FIGURE_EVIDENCE_FILTERS[0].1.contains("docx"), "a Word file is not evidence for a Figure request");
    }
}
