//! The window's page previews as raw pixels (68i).
//!
//! The window asks `render.get` and then reads the snapshot's preview. For a render whose preview the window reads (a board
//! page, a region, a thumbnail; `Work::Call::preview_pixels`) the RPC worker asks `render.pixels` instead: the host sends the
//! page's pixels as binary frames, with no PNG, base64 or hash. The worker gives the window the snapshot with a stand-in
//! preview (`pixels:<ticket>`, [`PIXELS_MIME`]) and holds the pixels until the window reads that preview, so the window's
//! code is the same for both. A render whose preview the window does not read (the flow search's page lookup) stays a
//! `render.get`.
//!
//! Held pixels are bounded by bytes ([`HELD_PIXEL_BYTES`]). A newer revision of a document drops the held pixels of its older
//! revisions: the window shows one revision and does not read an older one. A stand-in whose pixels went is rendered again
//! from the request it came from, which the worker keeps longer ([`HELD_REQUESTS`]). A stand-in is never sent to the host as
//! an asset hash. A host without raw pixels (another build, a renderer without them) answers `render.get` and its PNG.

use std::collections::VecDeque;

use designllm_bridge::Client;
use designllm_contracts::{AssetRef, DltError, DltResult, ErrorCode, RASTER_PIXEL_FORMAT};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::rpc::RpcPayload;

/// The stand-in preview type of a page rendered with `render.pixels`: the worker holds its pixels.
pub(crate) const PIXELS_MIME: &str = "image/x-designllm-rgba8-premultiplied";
const STAND_IN_PREFIX: &str = "pixels:";
const RENDER_PIXELS: &str = "render.pixels";
/// Raw pixels held for the window at most (a page texture is at most 16 M pixels, 64 MiB: `board::MAX_PAGE_PIXELS`).
pub(crate) const HELD_PIXEL_BYTES: usize = 256 * 1024 * 1024;
/// Render requests kept for the stand-ins whose pixels went, so they are rendered again instead of failing.
pub(crate) const HELD_REQUESTS: usize = 1024;
const ASSET_CHUNK: u64 = 4 * 1024 * 1024;

/// What the worker needs from the host: the bridge client, or a test's host.
pub(crate) trait Backend {
    fn call(&self, method: &str, params: Value) -> DltResult<Value>;
    /// The answer and the binary frames that came right before it (`Client::call_binary`).
    fn call_binary(&self, method: &str, params: Value) -> DltResult<(Value, Vec<Vec<u8>>)>;
}

impl Backend for Client {
    fn call(&self, method: &str, params: Value) -> DltResult<Value> {
        Client::call(self, method, params)
    }
    fn call_binary(&self, method: &str, params: Value) -> DltResult<(Value, Vec<Vec<u8>>)> {
        Client::call_binary(self, method, params)
    }
}

/// A picture for the decode thread.
pub(crate) enum Picture {
    Encoded { mime: String, bytes: Vec<u8> },
    /// Raw pixels of `render.pixels`: binary frame payloads, each a whole number of pixels.
    Pixels { size: [usize; 2], chunks: Vec<Vec<u8>> },
}

struct Held {
    stand_in: String,
    artifact_id: String,
    revision: u64,
    size: [usize; 2],
    chunks: Vec<Vec<u8>>,
    bytes: usize,
}

/// The pixels the worker holds for the window, and the requests they came from.
pub(crate) struct Previews {
    held: VecDeque<Held>,
    requests: VecDeque<(String, Value)>,
    budget: usize,
}

impl Default for Previews {
    fn default() -> Self {
        Self::with_budget(HELD_PIXEL_BYTES)
    }
}

impl Previews {
    pub(crate) fn with_budget(budget: usize) -> Self {
        Self { held: VecDeque::new(), requests: VecDeque::new(), budget }
    }

    /// The window's `render.get` whose preview it reads: `render.pixels`, whose pixels are held, or `render.get` from a host
    /// without raw pixels.
    pub(crate) fn render(&mut self, backend: &dyn Backend, ticket: u64, params: Value) -> DltResult<Value> {
        match backend.call_binary(RENDER_PIXELS, params.clone()) {
            Ok((mut value, chunks)) => {
                let (size, bytes) = take_pixels(&mut value, &chunks)?;
                let stand_in = format!("{STAND_IN_PREFIX}{ticket}");
                value["preview"] = json!({ "sha256": stand_in, "mime": PIXELS_MIME, "bytes": bytes });
                let artifact_id = value.get("artifact_id").and_then(Value::as_str).unwrap_or_default().to_string();
                let revision = value.get("revision").and_then(Value::as_u64).unwrap_or_default();
                self.hold(Held { stand_in: stand_in.clone(), artifact_id, revision, size, chunks, bytes });
                self.requests.push_back((stand_in, params));
                while self.requests.len() > HELD_REQUESTS {
                    self.requests.pop_front();
                }
                Ok(value)
            }
            Err(error) if error.code == ErrorCode::UnsupportedOperation => backend.call("render.get", params),
            Err(error) => Err(error),
        }
    }

    /// The picture of preview `asset`: held pixels, a stand-in rendered again, or an asset the host holds.
    pub(crate) fn picture(&mut self, backend: &dyn Backend, asset: &AssetRef) -> DltResult<Picture> {
        if asset.mime != PIXELS_MIME || !asset.sha256.starts_with(STAND_IN_PREFIX) {
            return read_asset(backend, asset).map(|bytes| Picture::Encoded { mime: asset.mime.clone(), bytes });
        }
        let request = self.requests.iter().position(|(id, _)| *id == asset.sha256).and_then(|at| self.requests.remove(at));
        if let Some(held) = self.held.iter().position(|held| held.stand_in == asset.sha256).and_then(|at| self.held.remove(at)) {
            return Ok(Picture::Pixels { size: held.size, chunks: held.chunks });
        }
        let (_, params) = request.ok_or_else(|| {
            DltError::new(ErrorCode::IntegrityError, "The page's pixels are gone and its render request is no longer known; render the page again")
        })?;
        match backend.call_binary(RENDER_PIXELS, params.clone()) {
            Ok((mut value, chunks)) => {
                let (size, _) = take_pixels(&mut value, &chunks)?;
                Ok(Picture::Pixels { size, chunks })
            }
            Err(error) if error.code == ErrorCode::UnsupportedOperation => png_page(backend, params),
            Err(error) => Err(error),
        }
    }

    /// Bytes of the pixels held now.
    #[cfg(test)]
    pub(crate) fn held_bytes(&self) -> usize {
        self.held.iter().map(|held| held.bytes).sum()
    }

    fn hold(&mut self, held: Held) {
        // The window shows one revision of a document: the pixels of its older revisions would not be read.
        self.held.retain(|old| old.artifact_id != held.artifact_id || old.revision >= held.revision);
        self.held.push_back(held);
        while self.held.iter().map(|held| held.bytes).sum::<usize>() > self.budget {
            self.held.pop_front();
        }
    }
}

/// Takes the `pixels` of a `render.pixels` answer out and checks them against the binary frames.
fn take_pixels(value: &mut Value, chunks: &[Vec<u8>]) -> DltResult<([usize; 2], usize)> {
    let mismatch = || DltError::new(ErrorCode::IntegrityError, "render.pixels answer and its pixels do not match");
    let pixels = value.as_object_mut().and_then(|object| object.remove("pixels")).ok_or_else(mismatch)?;
    let side = |key: &str| pixels.get(key).and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).filter(|n| *n > 0);
    let (Some(width), Some(height)) = (side("width"), side("height")) else { return Err(mismatch()) };
    let bytes: usize = chunks.iter().map(Vec::len).sum();
    if pixels.get("format").and_then(Value::as_str) != Some(RASTER_PIXEL_FORMAT)
        || width.checked_mul(height).and_then(|n| n.checked_mul(4)) != Some(bytes)
        || pixels.get("bytes").and_then(Value::as_u64) != Some(bytes as u64)
        || chunks.iter().any(|chunk| chunk.len() % 4 != 0)
    {
        return Err(mismatch());
    }
    Ok(([width, height], bytes))
}

/// The PNG preview of a render request, from a host without raw pixels.
fn png_page(backend: &dyn Backend, params: Value) -> DltResult<Picture> {
    let snapshot: designllm_contracts::RenderSnapshot = serde_json::from_value(backend.call("render.get", params)?)
        .map_err(|error| DltError::new(ErrorCode::IntegrityError, format!("render.get answer could not be read: {error}")))?;
    let asset = snapshot.preview.ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "render.get answered no preview"))?;
    Ok(Picture::Encoded { bytes: read_asset(backend, &asset)?, mime: asset.mime })
}

/// A picture as texture pixels.
pub(crate) fn decode(picture: Picture) -> Result<RpcPayload, DltError> {
    match picture {
        Picture::Encoded { mime, bytes } => {
            let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
                .map_err(|error| DltError::new(ErrorCode::IntegrityError, format!("Preview PNG could not be decoded: {error}")))?;
            let rgba = decoded.into_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            Ok(RpcPayload::Image { mime, image: eframe::egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()) })
        }
        Picture::Pixels { size, chunks } => {
            // Every chunk holds whole pixels (`take_pixels`), so the chunks are converted as they came, without joining them.
            let mut pixels = Vec::with_capacity(size[0] * size[1]);
            for chunk in &chunks {
                pixels.extend(chunk.chunks_exact(4).map(|p| eframe::egui::Color32::from_rgba_premultiplied(p[0], p[1], p[2], p[3])));
            }
            Ok(RpcPayload::Image { mime: PIXELS_MIME.into(), image: eframe::egui::ColorImage::new(size, pixels) })
        }
    }
}

/// An asset the host holds, in `asset.read` chunks, checked against its hash.
fn read_asset(backend: &dyn Backend, asset: &AssetRef) -> DltResult<Vec<u8>> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let capacity = usize::try_from(asset.bytes).map_err(|_| DltError::invalid("Asset is too large for this process"))?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut offset = 0_u64;
    while offset < asset.bytes {
        let maximum = ASSET_CHUNK.min(asset.bytes - offset);
        let result = backend.call("asset.read", json!({ "sha256": asset.sha256, "offset": offset, "max_bytes": maximum }))?;
        let actual_offset = result.get("offset").and_then(Value::as_u64).ok_or_else(|| DltError::invalid("asset.read omitted its offset"))?;
        let total = result.get("total_bytes").and_then(Value::as_u64).ok_or_else(|| DltError::invalid("asset.read omitted its byte count"))?;
        let encoded = result.get("base64").and_then(Value::as_str).ok_or_else(|| DltError::invalid("asset.read omitted its payload"))?;
        let eof = result.get("eof").and_then(Value::as_bool).ok_or_else(|| DltError::invalid("asset.read omitted eof"))?;
        let mime = result.get("mime").and_then(Value::as_str).ok_or_else(|| DltError::invalid("asset.read omitted mime"))?;
        if actual_offset != offset || total != asset.bytes || mime != asset.mime {
            return Err(DltError::new(ErrorCode::IntegrityError, "asset.read metadata changed during transfer"));
        }
        let chunk = STANDARD.decode(encoded).map_err(|error| DltError::invalid(format!("Invalid asset base64: {error}")))?;
        if chunk.is_empty() || chunk.len() as u64 > maximum {
            return Err(DltError::new(ErrorCode::IntegrityError, "asset.read returned an empty or oversized chunk"));
        }
        offset = offset.checked_add(chunk.len() as u64).ok_or_else(|| DltError::invalid("Asset offset overflow"))?;
        bytes.extend_from_slice(&chunk);
        if eof != (offset == asset.bytes) || offset > asset.bytes {
            return Err(DltError::new(ErrorCode::IntegrityError, "asset.read eof or length did not match the asset reference"));
        }
    }
    if format!("{:x}", Sha256::digest(&bytes)) != asset.sha256 {
        return Err(DltError::new(ErrorCode::IntegrityError, "Downloaded preview did not match its asset hash"));
    }
    Ok(bytes)
}

#[cfg(test)]
#[path = "pixels_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "pixels_host_tests.rs"]
mod host_tests;
