//! Timing of a UI-test window (68f): request round trips with the worker's queue wait and service time, the requests in
//! flight, stale hit replies, hit tests that returned without a request, `update` phases and texture uploads. Kept only in a
//! window started with `DESIGNLLM_UI_TEST_INPUT` (`UiTestInput::perf`); read with the test command `ui.perf` (`{"reset": true}`
//! also starts the counts again). Another window keeps none of it: its frame only checks that it has no test input.
//! Times are wall-clock CPU time of the window's own code: what the GPU does with a frame (paint, texture upload) is not in
//! them, and a reply is seen at the next frame, so round trips are rounded up to the window's frame interval.

use std::{
    collections::{BTreeMap, HashMap},
    time::{Duration, Instant},
};

use serde_json::{json, Value};

use crate::rpc::WorkTiming;

/// Samples kept per list; later ones are counted but not kept.
const MAX_SAMPLES: usize = 1024;

#[derive(Default)]
pub(crate) struct Perf {
    sent: HashMap<u64, (String, Instant)>,
    timings: HashMap<u64, WorkTiming>,
    methods: BTreeMap<String, MethodPerf>,
    counters: BTreeMap<&'static str, u64>,
    update_us: Vec<u32>,
    replies_us: Vec<u32>,
    draw_us: Vec<u32>,
    /// From one `update` start to the next: what the window's frame rate is.
    interval_us: Vec<u32>,
    last_frame_at: Option<Instant>,
    in_flight_max: usize,
    texture_uploads: u64,
    texture_bytes: u64,
    texture_us: Vec<u32>,
    /// The last frame that took synthetic input (`input`), and the times from it to marked moments (`mark`).
    input_at: Option<Instant>,
    after_input: BTreeMap<&'static str, Vec<u32>>,
}

#[derive(Default)]
struct MethodPerf {
    sent: u64,
    answered: u64,
    failed: u64,
    round_trip_us: Vec<u32>,
    wait_us: Vec<u32>,
    service_us: Vec<u32>,
    in_flight_at_send: Vec<u32>,
}

fn micros(duration: Duration) -> u32 {
    u32::try_from(duration.as_micros()).unwrap_or(u32::MAX)
}

fn keep(list: &mut Vec<u32>, value: u32) {
    if list.len() < MAX_SAMPLES {
        list.push(value);
    }
}

/// `{n, p50_ms, p95_ms, max_ms}` of a list of microseconds (nearest rank).
fn summary(list: &[u32]) -> Value {
    if list.is_empty() {
        return json!({ "n": 0 });
    }
    let mut sorted = list.to_vec();
    sorted.sort_unstable();
    let rank = |q: f64| sorted[((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len()) - 1];
    let ms = |us: u32| (f64::from(us) / 10.0).round() / 100.0;
    json!({ "n": sorted.len(), "p50_ms": ms(rank(0.5)), "p95_ms": ms(rank(0.95)), "max_ms": ms(sorted[sorted.len() - 1]) })
}

impl Perf {
    /// A request handed to the worker, with the requests already in flight.
    pub(crate) fn sent(&mut self, ticket: u64, method: &str, in_flight: usize) {
        self.sent.insert(ticket, (method.to_owned(), Instant::now()));
        self.in_flight_max = self.in_flight_max.max(in_flight + 1);
        let entry = self.methods.entry(method.to_owned()).or_default();
        entry.sent += 1;
        keep(&mut entry.in_flight_at_send, in_flight as u32);
    }

    /// When the worker took a request and finished it (sent before its reply).
    pub(crate) fn timed(&mut self, timing: WorkTiming) {
        if self.sent.contains_key(&timing.ticket) {
            self.timings.insert(timing.ticket, timing);
        }
    }

    /// A reply taken from the worker in `process_replies`.
    pub(crate) fn answered(&mut self, ticket: u64, failed: bool) {
        let Some((method, at)) = self.sent.remove(&ticket) else { return };
        let now = Instant::now();
        let timing = self.timings.remove(&ticket);
        let entry = self.methods.entry(method).or_default();
        entry.answered += 1;
        entry.failed += u64::from(failed);
        keep(&mut entry.round_trip_us, micros(now.saturating_duration_since(at)));
        if let Some(timing) = timing {
            keep(&mut entry.wait_us, micros(timing.started.saturating_duration_since(at)));
            keep(&mut entry.service_us, micros(timing.finished.saturating_duration_since(timing.started)));
        }
    }

    pub(crate) fn count(&mut self, name: &'static str) {
        *self.counters.entry(name).or_default() += 1;
    }

    /// A frame that took synthetic input.
    pub(crate) fn input(&mut self) {
        self.input_at = Some(Instant::now());
    }

    /// Counts `name` and keeps the time since the last synthetic input.
    pub(crate) fn mark(&mut self, name: &'static str) {
        self.count(name);
        if let Some(at) = self.input_at {
            keep(self.after_input.entry(name).or_default(), micros(at.elapsed()));
        }
    }

    /// One `update`: the time spent on the replies, and the whole.
    pub(crate) fn frame(&mut self, started: Instant, replies: Duration, total: Duration) {
        if let Some(last) = self.last_frame_at.replace(started) {
            keep(&mut self.interval_us, micros(started.saturating_duration_since(last)));
        }
        keep(&mut self.replies_us, micros(replies));
        keep(&mut self.update_us, micros(total));
        keep(&mut self.draw_us, micros(total.saturating_sub(replies)));
    }

    pub(crate) fn texture(&mut self, width: u32, height: u32, took: Duration) {
        keep(&mut self.texture_us, micros(took));
        self.mark("texture");
        self.texture_uploads += 1;
        self.texture_bytes += u64::from(width) * u64::from(height) * 4;
    }

    /// The counts since the window started or since the last `take`. Requests still in flight stay counted as sent.
    /// The report under `budget` bytes of JSON: when the kept samples would pass it, only the summaries go
    /// (`samples_dropped`).
    pub(crate) fn report(&self, budget: usize) -> Value {
        let mut report = self.full_report();
        if serde_json::to_vec(&report).map_or(true, |bytes| bytes.len() > budget) {
            strip_samples(&mut report);
            report["samples_dropped"] = json!(true);
        }
        report
    }

    fn full_report(&self) -> Value {
        let methods: serde_json::Map<String, Value> = self
            .methods
            .iter()
            .map(|(method, entry)| {
                (method.clone(), json!({
                    "sent": entry.sent,
                    "answered": entry.answered,
                    "failed": entry.failed,
                    "round_trip": summary(&entry.round_trip_us),
                    "queue_wait": summary(&entry.wait_us),
                    "service": summary(&entry.service_us),
                    "in_flight_at_send": summary_count(&entry.in_flight_at_send),
                    "round_trip_us": entry.round_trip_us,
                    "queue_wait_us": entry.wait_us,
                    "service_us": entry.service_us,
                }))
            })
            .collect();
        json!({
            "methods": methods,
            "counters": self.counters,
            "in_flight_now": self.sent.len(),
            "in_flight_max": self.in_flight_max,
            "frames": self.update_us.len(),
            "update": summary(&self.update_us),
            "replies": summary(&self.replies_us),
            "draw": summary(&self.draw_us),
            "update_us": self.update_us,
            "interval": summary(&self.interval_us),
            "interval_us": self.interval_us,
            "renderer": crate::renderer::selected().name(),
            "texture_uploads": self.texture_uploads,
            "texture_bytes": self.texture_bytes,
            "texture_upload": summary(&self.texture_us),
            "after_input": self.after_input.iter().map(|(name, list)| (name.to_string(), json!({ "summary": summary(list), "us": list }))).collect::<serde_json::Map<String, Value>>(),
        })
    }

    /// Starts the counts again; requests in flight stay tracked, so their replies still count.
    pub(crate) fn reset(&mut self) {
        let sent = std::mem::take(&mut self.sent);
        let timings = std::mem::take(&mut self.timings);
        *self = Self { sent, timings, ..Self::default() };
    }
}

/// Removes the sample lists (the keys ending in `_us`), keeping their summaries.
fn strip_samples(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|key, _| !key.ends_with("_us") && key != "us");
            map.values_mut().for_each(strip_samples);
        }
        Value::Array(list) => list.iter_mut().for_each(strip_samples),
        _ => {}
    }
}

/// `{n, p50, max}` of plain counts.
fn summary_count(list: &[u32]) -> Value {
    if list.is_empty() {
        return json!({ "n": 0 });
    }
    let mut sorted = list.to_vec();
    sorted.sort_unstable();
    json!({ "n": sorted.len(), "p50": sorted[(sorted.len() - 1) / 2], "max": sorted[sorted.len() - 1] })
}

impl super::DesktopApp {
    /// Counts `name` in a UI-test window's timing.
    pub(super) fn perf_count(&mut self, name: &'static str) {
        if let Some(test) = self.ui_test.as_mut() {
            test.perf.count(name);
        }
    }

    /// `perf_count` with the time since the last synthetic input.
    pub(super) fn perf_mark(&mut self, name: &'static str) {
        if let Some(test) = self.ui_test.as_mut() {
            test.perf.mark(name);
        }
    }

    pub(super) fn perf_texture(&mut self, width: u32, height: u32, took: Duration) {
        if let Some(test) = self.ui_test.as_mut() {
            test.perf.texture(width, height, took);
        }
    }
}

impl super::DesktopApp {
    /// Why a hit reply did not answer the scene on screen (`hit_test_scene_current`), counted in a UI-test window.
    pub(super) fn perf_count_stale_hit(&mut self, snapshot_id: &str, page_index: u32, transform_id: &str) {
        if self.ui_test.is_none() {
            return;
        }
        let revisions = self.active.as_ref().zip(self.artifact.as_ref()).zip(self.context.as_ref()).map(|((active, artifact), context)| {
            (active.revision, artifact.revision, context.revision)
        });
        let page = self.page_textures.get(&page_index);
        let reason = match (revisions, page) {
            (None, _) => "stale_hit_no_document",
            (_, None) => "stale_hit_no_page",
            (Some((active, artifact, _)), _) if active != artifact => "stale_hit_inspect_behind",
            (Some((active, _, context)), _) if active != context => "stale_hit_context_behind",
            (Some((active, _, _)), Some(page)) if page.revision != active => "stale_hit_page_behind",
            (_, Some(page)) if page.snapshot_id != snapshot_id => "stale_hit_new_snapshot",
            _ if self.page_transforms.get(&page_index).is_none_or(|transform| transform.id != transform_id) => "stale_hit_transform_moved",
            _ => "stale_hit_other",
        };
        self.perf_count(reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_over_its_budget_keeps_the_summaries_and_drops_the_samples() {
        let mut perf = Perf::default();
        for ticket in 0..2000 {
            perf.sent(ticket, "render.get", 0);
            perf.answered(ticket, false);
        }
        let full = perf.report(usize::MAX);
        assert_eq!(full["methods"]["render.get"]["round_trip_us"].as_array().map(Vec::len), Some(MAX_SAMPLES), "samples stop at the cap");
        let small = perf.report(1024);
        assert_eq!(small["samples_dropped"], true);
        assert!(small["methods"]["render.get"].get("round_trip_us").is_none());
        assert_eq!(small["methods"]["render.get"]["round_trip"]["n"], MAX_SAMPLES);
        assert_eq!(small["methods"]["render.get"]["sent"], 2000);
    }
}
