use designllm_application::create_workspace;
use designllm_contracts::{new_id, sha256, DltError, DltResult, ErrorCode, RpcRequest, PROJECT_ARTIFACTS_DIR, PROJECT_STORE_DIR, PROTOCOL_VERSION};
use designllm_core::{FaultPoint, Workspace};
use serde_json::{json, Value};
use std::{fs, path::{Path, PathBuf}};

fn call(workspace: &mut Workspace, method: &str, params: Value) -> DltResult<Value> {
    let response = workspace.dispatch(RpcRequest {
        protocol_version: PROTOCOL_VERSION, id: new_id(), method: method.to_owned(), params,
    });
    match response.error { Some(error) => Err(error), None => Ok(response.result.unwrap()) }
}

fn save(id: &Value, revision: u64) -> Value {
    json!({"request_id":new_id(),"artifact_id":id,"expected_revision":revision})
}

#[test]
fn project_files_reopen_in_another_workspace_and_keep_their_checked_destination() {
    let project = std::env::temp_dir().join(new_id());
    let store = project.join(PROJECT_STORE_DIR);
    let mut ws = create_workspace(&store).unwrap();
    let mut fresh = create_workspace(&project.join("fresh")).unwrap();
    for (kind, extension) in [("canvas","designllm"), ("web_code","designllm"), ("figure","designllm"),
        ("slides","pptx"), ("docx","docx"), ("hwpx","hwpx"), ("hwp","hwp")] {
        let doc = call(&mut ws, "artifact.create", json!({"actor":"test","request_id":new_id(),"kind":kind,"title":"보고서/초안"})).unwrap();
        let request = save(&doc["id"], 0);
        let result = call(&mut ws, "artifact.save", request.clone()).unwrap();
        assert_eq!(result, call(&mut ws, "artifact.save", request).unwrap());
        let path = Path::new(result["target_path"].as_str().unwrap());
        assert_eq!(path.parent().unwrap(), project.join(PROJECT_ARTIFACTS_DIR));
        assert_eq!(path.extension().unwrap(), extension);
        assert!(path.file_name().unwrap().to_string_lossy().starts_with("보고서_초안-"));
        let imported = call(&mut fresh, "artifact.open", json!({"actor":"test","request_id":new_id(),"path":path})).unwrap();
        assert_eq!(imported["kind"], kind);
        assert_eq!(call(&mut fresh, "artifact.save", save(&imported["id"], 0)).unwrap_err().code, ErrorCode::InvalidArgument);
    }
    drop(fresh);
    drop(ws);
    fs::remove_dir_all(project).unwrap();
}

#[test]
fn project_save_recovers_a_renamed_file_and_preserves_external_edits() {
    let project = std::env::temp_dir().join(new_id());
    let store = project.join(PROJECT_STORE_DIR);
    let mut ws = create_workspace(&store).unwrap();
    let doc = call(&mut ws, "artifact.create", json!({"actor":"test","request_id":new_id(),"kind":"figure","title":"Draft"})).unwrap();
    let id = &doc["id"];
    let folder = project.join(PROJECT_ARTIFACTS_DIR);
    fs::create_dir(&folder).unwrap();
    let suffix = sha256(id.as_str().unwrap().as_bytes());
    let collision = folder.join(format!("Draft-{}.designllm", &suffix[..16]));
    fs::write(&collision, b"existing user file").unwrap();
    assert_eq!(call(&mut ws, "artifact.save", save(id, 0)).unwrap_err().code, ErrorCode::AlreadyExists);
    assert_eq!(fs::read(&collision).unwrap(), b"existing user file");
    fs::remove_file(collision).unwrap();
    ws.set_fault_hook(Some(Box::new(|point| {
        if point == FaultPoint::NativeAfterRename { Err(DltError::new(ErrorCode::Internal, "interrupted after rename")) } else { Ok(()) }
    })));
    let first = save(id, 0);
    assert!(call(&mut ws, "artifact.save", first.clone()).is_err());
    drop(ws);
    let mut ws = create_workspace(&store).unwrap();
    let initial = call(&mut ws, "artifact.save", first).unwrap();
    let path = PathBuf::from(initial["target_path"].as_str().unwrap());
    let other_file = project.join("user-selected.designllm");
    let mut save_as = save(id, 0);
    save_as["target_path"] = json!(other_file);
    call(&mut ws, "artifact.save", save_as).unwrap();
    let workspace_id = ws.id().to_owned();
    call(&mut ws, "changes.apply", json!({"request_id":new_id(),"actor":"test","workspace_id":workspace_id,"artifact_id":id,
        "base_revision":0,"allowed_intents":["content"],"operations":[{"op":"set_caption","caption":"Updated"}]})).unwrap();
    assert_eq!(call(&mut ws, "artifact.save", save(id, 0)).unwrap_err().code, ErrorCode::RevisionConflict);
    let updated = call(&mut ws, "artifact.save", save(id, 1)).unwrap();
    assert_eq!(updated["target_path"], initial["target_path"]);
    assert_ne!(updated["target_hash"], initial["target_hash"]);
    assert_ne!(fs::read(&path).unwrap(), fs::read(&other_file).unwrap());

    let imported = call(&mut ws, "artifact.open", json!({"actor":"test","request_id":new_id(),"path":other_file})).unwrap();
    let imported_save = call(&mut ws, "artifact.save", save(&imported["id"], 0)).unwrap();
    assert_ne!(imported_save["target_path"], json!(other_file));
    fs::write(&path, b"saved outside DesignLLMTool").unwrap();
    assert_eq!(call(&mut ws, "artifact.save", save(id, 1)).unwrap_err().code, ErrorCode::SourceChanged);
    assert_eq!(fs::read(&path).unwrap(), b"saved outside DesignLLMTool");
    drop(ws);
    let moved = project.with_file_name(new_id());
    fs::rename(&project, &moved).unwrap();
    let mut ws = create_workspace(&moved.join(PROJECT_STORE_DIR)).unwrap();
    let state = call(&mut ws, "artifact.save_state", json!({"artifact_id":id})).unwrap();
    assert_eq!(state["target_path"], json!(moved.join(PROJECT_ARTIFACTS_DIR).join(path.file_name().unwrap())));
    assert_eq!(state["externally_modified"], true);
    assert_eq!(call(&mut ws, "artifact.save", save(id, 1)).unwrap_err().code, ErrorCode::SourceChanged);
    drop(ws);
    fs::remove_dir_all(moved).unwrap();
}
