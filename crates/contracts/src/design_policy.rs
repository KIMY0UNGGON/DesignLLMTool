//! Direct-formatting policy of a design-system sync. It travels only as the optional
//! `DesignSystemSetRequest.direct_formatting` (skipped when absent) and is never stored in a
//! snapshot, so documents, older request keys and digests stay as they were.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Persisted feature key of an artifact that links or publishes a shared design system.
pub const FEATURE_ARTIFACT_DESIGN_SYSTEM: &str = "artifact.design_system.v1";

/// What a design-system sync does with the consumer's own direct formatting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DirectFormattingPolicy {
    /// Direct formatting stays as it is.
    Keep,
    /// Direct formatting is removed, so the published style values apply.
    Clear,
}

#[cfg(test)]
mod tests {
    use super::DirectFormattingPolicy;
    use crate::*;
    use std::sync::Mutex;

    /// What `sync_design_system` received: the artifact id, the token keys, and the design system.
    type SyncCall = (Id, Vec<String>, Option<DesignSystemRef>);

    /// Only `sync_design_system` is real: it records what it received and refuses with a marker message.
    /// The other required methods are never called.
    struct SyncProbe {
        sync_calls: Mutex<Vec<SyncCall>>,
    }

    /// The required methods of both probes; none of them is called.
    macro_rules! unused_engine_methods {
        () => {
        fn id(&self) -> &'static str { "probe" }
        fn kinds(&self) -> Vec<ArtifactKind> { vec![ArtifactKind::Canvas] }
        fn create(&self, _: &CreateRequest, _: &EngineContext<'_>) -> DltResult<Artifact> { unimplemented!() }
        fn open(&self, _: &OpenInput, _: &EngineContext<'_>) -> DltResult<Artifact> { unimplemented!() }
        fn capabilities(&self, _: &Artifact) -> CapabilityReport { unimplemented!() }
        fn supported_persisted_features(&self) -> BTreeSet<String> { BTreeSet::new() }
        fn persisted_features(&self, _: &Artifact) -> DltResult<BTreeSet<String>> { unimplemented!() }
        fn inspect(&self, _: &Artifact, _: &[Id]) -> DltResult<Value> { unimplemented!() }
        fn prepare_change(&self, _: &Artifact, _: &[Operation], _: &EngineContext<'_>) -> DltResult<PreparedChange> { unimplemented!() }
        fn validate(&self, _: &Artifact, _: &EngineContext<'_>) -> Vec<Diagnostic> { unimplemented!() }
        fn content_digest(&self, _: &Artifact) -> DltResult<ContentDigest> { unimplemented!() }
        fn layout(&self, _: &Artifact, _: &EngineContext<'_>) -> DltResult<RenderScene> { unimplemented!() }
        fn encode_native(&self, _: &Artifact, _: &EngineContext<'_>) -> DltResult<EncodedOutput> { unimplemented!() }
        fn verify_saved(&self, _: &Artifact, _: &[u8], _: &EngineContext<'_>) -> DltResult<PreservationVerification> { unimplemented!() }
        fn export(&self, _: &Artifact, _: &ExportRequest, _: &EngineContext<'_>) -> DltResult<EncodedOutput> { unimplemented!() }
        };
    }

    impl EngineAdapter for SyncProbe {
        unused_engine_methods!();
        fn sync_design_system(&self, artifact: &Artifact, tokens: &BTreeMap<String, TokenDef>, design_system: Option<&DesignSystemRef>) -> DltResult<PreparedChange> {
            self.sync_calls.lock().unwrap().push((artifact.id.clone(), tokens.keys().cloned().collect(), design_system.cloned()));
            Err(DltError::invalid("sync_design_system reached"))
        }
    }

    /// An engine that keeps the default `sync_design_system`.
    struct NoSyncProbe;

    impl EngineAdapter for NoSyncProbe {
        unused_engine_methods!();
    }

    #[test]
    fn keep_delegates_to_sync_design_system_and_clear_is_refused_without_calling_it() {
        let probe = SyncProbe { sync_calls: Mutex::new(Vec::new()) };
        let artifact = Artifact::new(ArtifactKind::Canvas, "policy probe", "probe", ArtifactBody::Canvas(CanvasDocument::default()));
        let tokens = BTreeMap::from([("t".to_string(), TokenDef { name: "Token".into(), values: TokenValues::Color { light: "#000000".into(), dark: "#ffffff".into() } })]);
        let design_system = DesignSystemRef { artifact_id: "system".into(), revision: 3 };

        let kept = probe.sync_design_system_with_policy(&artifact, &tokens, Some(&design_system), DirectFormattingPolicy::Keep).err().expect("Keep must reach the engine");
        assert_eq!(kept.message, "sync_design_system reached");
        assert_eq!(*probe.sync_calls.lock().unwrap(), [(artifact.id.clone(), vec!["t".to_string()], Some(design_system.clone()))], "Keep passes its arguments on unchanged");

        let cleared = probe.sync_design_system_with_policy(&artifact, &tokens, Some(&design_system), DirectFormattingPolicy::Clear).err().expect("Clear must be refused");
        assert_eq!(cleared.code, ErrorCode::UnsupportedOperation);
        assert_eq!(probe.sync_calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn keep_on_an_engine_without_sync_is_unsupported() {
        let artifact = Artifact::new(ArtifactKind::Canvas, "policy probe", "probe", ArtifactBody::Canvas(CanvasDocument::default()));
        for policy in [DirectFormattingPolicy::Keep, DirectFormattingPolicy::Clear] {
            let refused = NoSyncProbe.sync_design_system_with_policy(&artifact, &BTreeMap::new(), None, policy).err().expect("no sync");
            assert_eq!(refused.code, ErrorCode::UnsupportedOperation, "{policy:?}");
        }
    }

    #[test]
    fn set_request_direct_formatting_is_optional_snake_case_and_absent_when_unset() {
        let legacy = serde_json::json!({
            "request_id":"r","actor":"a","workspace_id":"w","artifact_id":"c","expected_revision":1,
            "design_system":"p","allowed_intents":["design"]
        });
        let parsed: DesignSystemSetRequest = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(parsed.direct_formatting, None);
        assert_eq!(serde_json::to_value(&parsed).unwrap(), legacy, "an older request serializes as before");

        for (policy, wire) in [(DirectFormattingPolicy::Keep, "keep"), (DirectFormattingPolicy::Clear, "clear")] {
            let mut sent = legacy.clone();
            sent["direct_formatting"] = wire.into();
            let parsed: DesignSystemSetRequest = serde_json::from_value(sent.clone()).unwrap();
            assert_eq!(parsed.direct_formatting, Some(policy));
            assert_eq!(serde_json::to_value(&parsed).unwrap(), sent);
        }
        let mut wrong = legacy;
        wrong["direct_formatting"] = "Keep".into();
        assert!(serde_json::from_value::<DesignSystemSetRequest>(wrong).is_err());
    }
}
