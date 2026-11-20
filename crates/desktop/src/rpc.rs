use std::{
    path::Path,
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use designllm_bridge::{home, home::HomeRequest, Client};
use designllm_contracts::{AssetRef, DltError, DltResult, ErrorCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const ASSET_CHUNK: u64 = 4 * 1024 * 1024;

pub struct RpcWorker {
    send: Sender<Work>,
    receive: Receiver<RpcReply>,
}

pub(crate) enum Work {
    Call {
        ticket: u64,
        method: String,
        params: Value,
    },
    ReadAsset {
        ticket: u64,
        asset: AssetRef,
    },
    Home {
        ticket: u64,
        request: HomeRequest,
    },
}

pub enum RpcPayload {
    Json(Value),
    Asset { mime: String, width: u32, height: u32, rgba: Vec<u8> },
}

pub struct RpcReply {
    pub ticket: u64,
    pub result: Result<RpcPayload, DltError>,
}

impl RpcWorker {
    pub fn spawn(workspace: &Path) -> DltResult<Self> {
        let client = Client::gui(workspace);
        let (send, work) = mpsc::channel();
        let (reply, receive) = mpsc::channel();
        thread::Builder::new()
            .name("designllm-rpc".into())
            .spawn(move || {
                while let Ok(task) = work.recv() {
                    let response = match task {
                        Work::Call {
                            ticket,
                            method,
                            params,
                        } => {
                            let result = client
                                .call(&method, params)
                                .map(RpcPayload::Json);
                            RpcReply {
                                ticket,
                                result,
                            }
                        }
                        Work::Home { ticket, request } => {
                            let result = home::create_from_home(&client, &request).map(|outcome| {
                                RpcPayload::Json(json!({ "artifact": outcome.artifact, "job": outcome.job, "sources": outcome.sources }))
                            });
                            RpcReply {
                                ticket,
                                result,
                            }
                        }
                        Work::ReadAsset { ticket, asset } => {
                            let result = read_asset(&client, &asset).and_then(|bytes| {
                                let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
                                    .map_err(|error| DltError::new(ErrorCode::IntegrityError, format!("Preview PNG could not be decoded: {error}")))?;
                                let rgba = decoded.to_rgba8();
                                Ok(RpcPayload::Asset {
                                    mime: asset.mime,
                                    width: rgba.width(),
                                    height: rgba.height(),
                                    rgba: rgba.into_raw(),
                                })
                            });
                            RpcReply {
                                ticket,
                                result,
                            }
                        }
                    };
                    if reply.send(response).is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| DltError::new(ErrorCode::Internal, error.to_string()))?;
        Ok(Self { send, receive })
    }

    /// A worker with no thread behind it. A test reads the requests the window sends from the
    /// returned receiver and answers them by hand through the returned sender.
    #[cfg(test)]
    pub(crate) fn detached() -> (Self, Sender<RpcReply>, Receiver<Work>) {
        let (send, work) = mpsc::channel();
        let (reply, receive) = mpsc::channel();
        (Self { send, receive }, reply, work)
    }

    pub fn call(&self, ticket: u64, method: &str, params: Value) -> DltResult<()> {
        self.send
            .send(Work::Call {
                ticket,
                method: method.into(),
                params,
            })
            .map_err(|error| DltError::new(ErrorCode::Internal, error.to_string()))
    }

    pub(crate) fn create_from_home(&self, ticket: u64, request: HomeRequest) -> DltResult<()> {
        self.send
            .send(Work::Home { ticket, request })
            .map_err(|error| DltError::new(ErrorCode::Internal, error.to_string()))
    }

    pub fn read_asset(&self, ticket: u64, asset: AssetRef) -> DltResult<()> {
        self.send
            .send(Work::ReadAsset { ticket, asset })
            .map_err(|error| DltError::new(ErrorCode::Internal, error.to_string()))
    }

    pub fn poll(&self) -> impl Iterator<Item = RpcReply> + '_ {
        self.receive.try_iter()
    }
}

fn read_asset(client: &Client, asset: &AssetRef) -> DltResult<Vec<u8>> {
    let capacity = usize::try_from(asset.bytes)
        .map_err(|_| DltError::invalid("Asset is too large for this process"))?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut offset = 0_u64;

    while offset < asset.bytes {
        let maximum = ASSET_CHUNK.min(asset.bytes - offset);
        let result = client.call(
            "asset.read",
            json!({
                "sha256": asset.sha256,
                "offset": offset,
                "max_bytes": maximum,
            }),
        )?;
        let actual_offset = result
            .get("offset")
            .and_then(Value::as_u64)
            .ok_or_else(|| DltError::invalid("asset.read omitted its offset"))?;
        let total = result
            .get("total_bytes")
            .and_then(Value::as_u64)
            .ok_or_else(|| DltError::invalid("asset.read omitted its byte count"))?;
        let encoded = result
            .get("base64")
            .and_then(Value::as_str)
            .ok_or_else(|| DltError::invalid("asset.read omitted its payload"))?;
        let eof = result
            .get("eof")
            .and_then(Value::as_bool)
            .ok_or_else(|| DltError::invalid("asset.read omitted eof"))?;
        let mime = result
            .get("mime")
            .and_then(Value::as_str)
            .ok_or_else(|| DltError::invalid("asset.read omitted mime"))?;
        if actual_offset != offset || total != asset.bytes || mime != asset.mime {
            return Err(DltError::new(
                ErrorCode::IntegrityError,
                "asset.read metadata changed during transfer",
            ));
        }

        let chunk = STANDARD
            .decode(encoded)
            .map_err(|error| DltError::invalid(format!("Invalid asset base64: {error}")))?;
        if chunk.is_empty() || chunk.len() as u64 > maximum {
            return Err(DltError::new(
                ErrorCode::IntegrityError,
                "asset.read returned an empty or oversized chunk",
            ));
        }
        offset = offset
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| DltError::invalid("Asset offset overflow"))?;
        bytes.extend_from_slice(&chunk);
        if eof != (offset == asset.bytes) || offset > asset.bytes {
            return Err(DltError::new(
                ErrorCode::IntegrityError,
                "asset.read eof or length did not match the asset reference",
            ));
        }
    }

    let actual_hash = format!("{:x}", Sha256::digest(&bytes));
    if actual_hash != asset.sha256 {
        return Err(DltError::new(
            ErrorCode::IntegrityError,
            "Downloaded preview did not match its asset hash",
        ));
    }
    Ok(bytes)
}