use std::{
    collections::VecDeque,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, OnceLock,
    },
    thread,
    time::Instant,
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
    /// Once `enable_timing` is called (a UI-test window's `perf`, 68f), the worker sends when it took and finished each
    /// request, before its reply.
    timed: Arc<AtomicBool>,
    timing: Receiver<WorkTiming>,
    /// The window's context: the worker asks it for a frame after each reply, so a reply is handled at once instead of at the
    /// next frame the window would draw anyway (up to 100 ms later while requests are in flight, 68f).
    waker: Arc<OnceLock<eframe::egui::Context>>,
}

/// When the worker took request `ticket` off its queue and when it finished it.
pub(crate) struct WorkTiming {
    pub(crate) ticket: u64,
    pub(crate) started: Instant,
    pub(crate) finished: Instant,
}

impl Work {
    #[cfg(test)]
    fn ticket(&self) -> u64 {
        match self {
            Work::Call { ticket, .. } | Work::ReadAsset { ticket, .. } | Work::Home { ticket, .. } => *ticket,
        }
    }

    /// A request a click or the board's view waits on: it goes before the reads queued ahead of it (`overtakable`), in
    /// the order these were sent (68f 3-1). None of them changes a revision.
    fn interactive(&self) -> bool {
        matches!(self, Work::Call { method, .. } if matches!(method.as_str(), "render.hit_test" | "view.transform.set" | "selection.set"))
    }

    /// A read an interactive request may go before: a page render, a picture, a state read. Any other request (a change, a
    /// reservation, an undo, a save) is a barrier: what was sent after it runs after it, as sent.
    fn overtakable(&self) -> bool {
        match self {
            Work::ReadAsset { .. } => true,
            Work::Call { method, .. } => matches!(
                method.as_str(),
                "render.get" | "context.read" | "artifact.inspect" | "artifact.validate" | "workspace.list" | "events.poll"
                    | "ui.state.publish" | "asset.lookup"
            ),
            Work::Home { .. } => false,
        }
    }
}

/// How many requests in a row may go before the oldest queued one, so that a run of board transforms (one a frame while a
/// zoom settles) cannot hold a page render back for long.
const MAX_OVERTAKES: u32 = 8;

/// The next request to run: the first interactive one with only reads queued ahead of it, else the oldest. `overtakes`
/// counts the requests that went before the oldest one in a row.
fn next_work(queue: &mut VecDeque<Work>, overtakes: &mut u32) -> Option<Work> {
    let mut index = 0;
    if *overtakes < MAX_OVERTAKES {
        for (at, work) in queue.iter().enumerate() {
            if work.interactive() {
                index = at;
                break;
            }
            if !work.overtakable() {
                break;
            }
        }
    }
    *overtakes = if index == 0 { 0 } else { *overtakes + 1 };
    queue.remove(index)
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
    /// A picture the worker decoded and converted to egui's pixels, so the window's frame only hands it to a texture (68f
    /// 6-2: converting a large page in the frame took 20-30 ms).
    Image { mime: String, image: eframe::egui::ColorImage },
}

impl RpcPayload {
    /// The PNG picture of an `Image` answer; None for another answer, another type or an empty picture.
    pub(crate) fn png_image(self) -> Option<eframe::egui::ColorImage> {
        match self {
            RpcPayload::Image { mime, image } if mime == "image/png" && image.width() > 0 && image.height() > 0 => Some(image),
            _ => None,
        }
    }
}

pub struct RpcReply {
    pub ticket: u64,
    pub result: Result<RpcPayload, DltError>,
}

/// Where the worker threads hand a finished request: its reply, its timing (once `enable_timing` was called), and a frame
/// request to the window.
#[derive(Clone)]
struct Outbox {
    reply: Sender<RpcReply>,
    timing: Sender<WorkTiming>,
    timed: Arc<AtomicBool>,
    waker: Arc<OnceLock<eframe::egui::Context>>,
}

impl Outbox {
    /// False once the window is gone.
    fn deliver(&self, ticket: u64, started: Instant, result: Result<RpcPayload, DltError>) -> bool {
        if self.timed.load(Ordering::Relaxed) {
            let _ = self.timing.send(WorkTiming { ticket, started, finished: Instant::now() });
        }
        if self.reply.send(RpcReply { ticket, result }).is_err() {
            return false;
        }
        if let Some(ctx) = self.waker.get() {
            ctx.request_repaint();
        }
        true
    }
}

/// A preview PNG as texture pixels.
fn decode_preview(mime: String, bytes: &[u8]) -> Result<RpcPayload, DltError> {
    let decoded = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .map_err(|error| DltError::new(ErrorCode::IntegrityError, format!("Preview PNG could not be decoded: {error}")))?;
    let rgba = decoded.into_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let image = eframe::egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    Ok(RpcPayload::Image { mime, image })
}

impl RpcWorker {
    pub fn spawn(workspace: &Path) -> DltResult<Self> {
        let client = Client::gui(workspace);
        let (send, work) = mpsc::channel::<Work>();
        let (reply, receive) = mpsc::channel();
        let (timing_send, timing) = mpsc::channel();
        let timed = Arc::new(AtomicBool::new(false));
        let waker = Arc::new(OnceLock::<eframe::egui::Context>::new());
        let outbox = Outbox { reply, timing: timing_send, timed: Arc::clone(&timed), waker: Arc::clone(&waker) };
        // A preview is decoded on its own thread, so the requests queued behind its read (a click's) do not wait for the
        // decode as well (68f 3-1). Previews keep their order; one waits while another decodes, so at most three pictures
        // (read, waiting, decoding) are held at once.
        let (decode, decode_work) = mpsc::sync_channel::<(u64, Instant, String, Vec<u8>)>(1);
        let decoder = outbox.clone();
        thread::Builder::new()
            .name("designllm-decode".into())
            .spawn(move || {
                for (ticket, started, mime, bytes) in decode_work {
                    if !decoder.deliver(ticket, started, decode_preview(mime, &bytes)) {
                        break;
                    }
                }
            })
            .map_err(|error| DltError::new(ErrorCode::Internal, error.to_string()))?;
        thread::Builder::new()
            .name("designllm-rpc".into())
            .spawn(move || {
                let mut queue = VecDeque::new();
                let mut overtakes = 0;
                loop {
                    if queue.is_empty() {
                        match work.recv() {
                            Ok(task) => queue.push_back(task),
                            Err(_) => break,
                        }
                    }
                    queue.extend(work.try_iter());
                    let Some(task) = next_work(&mut queue, &mut overtakes) else { continue };
                    let started = Instant::now();
                    let (ticket, result) = match task {
                        Work::Call { ticket, method, params } => (ticket, client.call(&method, params).map(RpcPayload::Json)),
                        Work::Home { ticket, request } => (
                            ticket,
                            home::create_from_home(&client, &request).map(|outcome| {
                                RpcPayload::Json(json!({ "artifact": outcome.artifact, "job": outcome.job, "sources": outcome.sources }))
                            }),
                        ),
                        Work::ReadAsset { ticket, asset } => match read_asset(&client, &asset) {
                            Ok(bytes) => {
                                if decode.send((ticket, started, asset.mime, bytes)).is_err() {
                                    break;
                                }
                                continue;
                            }
                            Err(error) => (ticket, Err(error)),
                        },
                    };
                    if !outbox.deliver(ticket, started, result) {
                        break;
                    }
                }
            })
            .map_err(|error| DltError::new(ErrorCode::Internal, error.to_string()))?;
        Ok(Self { send, receive, timed, timing, waker })
    }

    /// The context whose window handles the replies (set once, at the first frame).
    pub(crate) fn set_waker(&self, ctx: &eframe::egui::Context) {
        let _ = self.waker.set(ctx.clone());
    }

    /// From now on the worker reports when it took and finished each request (`timings`).
    pub(crate) fn enable_timing(&self) {
        self.timed.store(true, Ordering::Relaxed);
    }

    pub(crate) fn timings(&self) -> impl Iterator<Item = WorkTiming> + '_ {
        self.timing.try_iter()
    }

    /// A worker with no thread behind it. A test reads the requests the window sends from the
    /// returned receiver and answers them by hand through the returned sender.
    #[cfg(test)]
    pub(crate) fn detached() -> (Self, Sender<RpcReply>, Receiver<Work>) {
        let (send, work) = mpsc::channel();
        let (reply, receive) = mpsc::channel();
        let (_, timing) = mpsc::channel();
        (Self { send, receive, timed: Arc::new(AtomicBool::new(false)), timing, waker: Arc::new(OnceLock::new()) }, reply, work)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn call(ticket: u64, method: &str) -> Work {
        Work::Call { ticket, method: method.into(), params: Value::Null }
    }

    #[test]
    fn a_click_s_requests_go_before_queued_renders_in_the_order_they_were_sent() {
        let queue: VecDeque<Work> = [
            call(1, "render.get"),
            Work::ReadAsset { ticket: 2, asset: AssetRef { sha256: "0".into(), mime: "image/png".into(), bytes: 1 } },
            call(3, "view.transform.set"),
            call(4, "render.hit_test"),
            call(5, "context.read"),
            call(6, "selection.set"),
        ]
        .into_iter()
        .collect();
        assert_eq!(order_of(queue), [3, 4, 6, 1, 2, 5]);
    }

    fn order_of(mut queue: VecDeque<Work>) -> Vec<u64> {
        let mut overtakes = 0;
        std::iter::from_fn(|| next_work(&mut queue, &mut overtakes)).map(|work| work.ticket()).collect()
    }

    #[test]
    fn a_change_queued_before_a_click_s_requests_runs_first() {
        let queue: VecDeque<Work> = [
            call(1, "render.get"),
            call(2, "changes.apply"),
            call(3, "selection.set"),
            call(4, "render.hit_test"),
            call(5, "targets.reserve"),
            call(6, "render.get"),
            call(7, "view.transform.set"),
        ]
        .into_iter()
        .collect();
        assert_eq!(order_of(queue), [1, 2, 3, 4, 5, 7, 6], "no request goes before a change or a reservation sent ahead of it");
    }

    #[test]
    fn a_run_of_board_transforms_lets_the_oldest_render_through() {
        let queue: VecDeque<Work> = std::iter::once(call(0, "render.get")).chain((1..=12).map(|ticket| call(ticket, "view.transform.set"))).collect();
        let order = order_of(queue);
        let render_at = order.iter().position(|ticket| *ticket == 0).expect("the render runs");
        assert_eq!(render_at, MAX_OVERTAKES as usize, "the render waits for at most {MAX_OVERTAKES} transforms: {order:?}");
    }
}
