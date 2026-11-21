use std::cell::RefCell;

use base64::{engine::general_purpose::STANDARD, Engine as _};

use super::*;

/// A host for the worker: `render.pixels` answers `side`×`side` pixels in chunks of `chunk` bytes, or (`old`) is unknown and
/// `render.get` answers a PNG preview. Every call is recorded.
struct FakeHost {
    side: usize,
    chunk: usize,
    old: bool,
    calls: RefCell<Vec<(String, Value)>>,
    png: Vec<u8>,
}

impl FakeHost {
    fn new(side: usize) -> Self {
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("a PNG");
        Self { side, chunk: 4 * 1024, old: false, calls: RefCell::new(Vec::new()), png }
    }
    fn count(&self, method: &str) -> usize {
        self.calls.borrow().iter().filter(|(name, _)| name == method).count()
    }
    fn png_ref(&self) -> AssetRef {
        AssetRef { sha256: format!("{:x}", Sha256::digest(&self.png)), mime: "image/png".into(), bytes: self.png.len() as u64 }
    }
}

impl Backend for FakeHost {
    fn call(&self, method: &str, params: Value) -> DltResult<Value> {
        self.calls.borrow_mut().push((method.into(), params.clone()));
        match method {
            "render.get" => Ok(json!({
                "id": "png", "artifact_id": params["artifact_id"], "revision": params["expected_revision"], "profile": { "dpi": 96, "transparent": false },
                "scene": { "pages": [], "font_fingerprint": "", "diagnostics": [], "metadata": {}, "links": [] },
                "preview": self.png_ref(), "preview_transform": null,
            })),
            "asset.read" => {
                assert_eq!(params["sha256"], json!(self.png_ref().sha256), "only a held asset is read");
                Ok(json!({ "sha256": params["sha256"], "mime": "image/png", "offset": 0, "bytes": self.png.len(), "eof": true, "total_bytes": self.png.len(), "base64": STANDARD.encode(&self.png) }))
            }
            other => panic!("unexpected {other}"),
        }
    }
    fn call_binary(&self, method: &str, params: Value) -> DltResult<(Value, Vec<Vec<u8>>)> {
        self.calls.borrow_mut().push((method.into(), params.clone()));
        if self.old {
            return Err(DltError::unsupported("unknown method \"render.pixels\""));
        }
        let bytes = vec![200u8; self.side * self.side * 4];
        let value = json!({
            "id": format!("s{}", params["page_index"]), "artifact_id": params["artifact_id"], "revision": params["expected_revision"], "preview": null,
            "pixels": { "width": self.side, "height": self.side, "format": RASTER_PIXEL_FORMAT, "bytes": bytes.len() },
        });
        Ok((value, bytes.chunks(self.chunk).map(<[u8]>::to_vec).collect()))
    }
}

fn request(page: u32, revision: u64) -> Value {
    json!({ "artifact_id": "a1", "expected_revision": revision, "page_index": page })
}

fn preview_of(answer: &Value) -> AssetRef {
    serde_json::from_value(answer["preview"].clone()).expect("a preview the window reads")
}

#[test]
fn a_page_s_pixels_reach_the_window_under_a_stand_in_without_an_asset_read() {
    let host = FakeHost::new(64);
    let mut previews = Previews::default();
    let answer = previews.render(&host, 7, request(0, 1)).expect("rendered");
    assert!(answer.get("pixels").is_none(), "the window sees no pixels field");
    let preview = preview_of(&answer);
    assert_eq!((preview.mime.as_str(), preview.bytes), (PIXELS_MIME, 64 * 64 * 4));
    let picture = previews.picture(&host, &preview).expect("held");
    assert!(matches!(&picture, Picture::Pixels { chunks, .. } if chunks.len() == 4), "the frames are kept as they came");
    let RpcPayload::Image { image, .. } = decode(picture).expect("decoded") else { panic!("an image") };
    assert_eq!(image.size, [64, 64]);
    assert_eq!(image.pixels[0], eframe::egui::Color32::from_rgba_premultiplied(200, 200, 200, 200));
    assert_eq!((host.count("render.pixels"), host.count("asset.read"), host.count("render.get")), (1, 0, 0));
    assert_eq!(previews.held_bytes(), 0, "read once, then let go");
}

#[test]
fn many_visible_pages_are_all_drawn_and_a_stand_in_never_goes_to_the_host() {
    // Four pages fit the budget: nothing is rendered twice.
    let host = FakeHost::new(64);
    let mut previews = Previews::default();
    let answers: Vec<Value> = (0..4).map(|page| previews.render(&host, u64::from(page), request(page, 1)).unwrap()).collect();
    for answer in &answers {
        assert!(matches!(previews.picture(&host, &preview_of(answer)).unwrap(), Picture::Pixels { .. }));
    }
    assert_eq!(host.count("render.pixels"), 4);

    // Forty pages over a budget of three: the window sends every render before it reads a preview (one frame asks all
    // visible pages, the worker's queue is in order). The pixels that went are rendered again; no asset.read is sent.
    let host = FakeHost::new(64);
    let mut previews = Previews::with_budget(3 * 64 * 64 * 4);
    let answers: Vec<Value> = (0..40).map(|page| previews.render(&host, u64::from(page), request(page, 1)).unwrap()).collect();
    assert_eq!(previews.held_bytes(), 3 * 64 * 64 * 4);
    for answer in &answers {
        assert!(matches!(previews.picture(&host, &preview_of(answer)).unwrap(), Picture::Pixels { .. }));
    }
    assert_eq!((host.count("render.pixels"), host.count("asset.read")), (40 + 37, 0));
    let again: Vec<u32> = host.calls.borrow()[40..].iter().map(|(_, params)| params["page_index"].as_u64().unwrap() as u32).collect();
    assert_eq!(again, (0..37).collect::<Vec<_>>(), "each page is asked again with its own request");
}

#[test]
fn a_stand_in_whose_request_is_no_longer_known_is_an_error_not_an_asset_read() {
    let host = FakeHost::new(8);
    let mut previews = Previews::with_budget(0);
    let first = previews.render(&host, 0, request(0, 1)).unwrap();
    for ticket in 1..=(HELD_REQUESTS as u64) {
        previews.render(&host, ticket, request(1, 1)).unwrap();
    }
    let error = previews.picture(&host, &preview_of(&first)).err().expect("refused");
    assert_eq!(error.code, ErrorCode::IntegrityError);
    assert_eq!(host.count("asset.read"), 0);
}

#[test]
fn a_newer_revision_lets_the_older_pixels_go_and_an_older_stand_in_is_rendered_again() {
    let host = FakeHost::new(16);
    let mut previews = Previews::default();
    let old = previews.render(&host, 1, request(0, 1)).unwrap();
    let other_document = previews.render(&host, 2, json!({ "artifact_id": "b1", "expected_revision": 0, "page_index": 0 })).unwrap();
    previews.render(&host, 3, request(0, 2)).unwrap();
    assert_eq!(previews.held_bytes(), 2 * 16 * 16 * 4, "revision 1 of a1 went; b1 stays");
    assert!(matches!(previews.picture(&host, &preview_of(&other_document)).unwrap(), Picture::Pixels { .. }));
    assert!(matches!(previews.picture(&host, &preview_of(&old)).unwrap(), Picture::Pixels { .. }));
    assert_eq!(host.count("render.pixels"), 4, "the old page is asked again");
}

#[test]
fn a_host_without_raw_pixels_answers_render_get_and_its_png() {
    let mut host = FakeHost::new(8);
    host.old = true;
    let mut previews = Previews::default();
    let answer = previews.render(&host, 1, request(0, 1)).expect("render.get");
    let preview = preview_of(&answer);
    assert_eq!(preview.mime, "image/png");
    let picture = previews.picture(&host, &preview).expect("asset.read");
    let RpcPayload::Image { image, mime } = decode(picture).expect("decoded") else { panic!("an image") };
    assert_eq!((mime.as_str(), image.size), ("image/png", [2, 2]));
    assert_eq!((host.count("render.pixels"), host.count("render.get"), host.count("asset.read")), (1, 1, 1));

    // A stand-in kept from before the host went old is drawn from render.get's PNG.
    host.old = false;
    let mut previews = Previews::with_budget(0);
    let answer = previews.render(&host, 2, request(0, 1)).unwrap();
    host.old = true;
    assert!(matches!(previews.picture(&host, &preview_of(&answer)).unwrap(), Picture::Encoded { .. }));
}

#[test]
fn pixels_that_do_not_match_their_answer_are_refused() {
    struct Wrong(Value, Vec<Vec<u8>>);
    impl Backend for Wrong {
        fn call(&self, _: &str, _: Value) -> DltResult<Value> {
            unreachable!()
        }
        fn call_binary(&self, _: &str, _: Value) -> DltResult<(Value, Vec<Vec<u8>>)> {
            Ok((self.0.clone(), self.1.clone()))
        }
    }
    let answer = |width: u64, height: u64, bytes: u64| json!({ "id": "s", "pixels": { "width": width, "height": height, "format": RASTER_PIXEL_FORMAT, "bytes": bytes } });
    for host in [
        Wrong(answer(2, 1, 8), vec![vec![0; 4]]),
        Wrong(answer(2, 1, 4), vec![vec![0; 4]]),
        Wrong(answer(1, 1, 4), vec![vec![0; 3], vec![0]]),
        Wrong(json!({ "id": "s" }), vec![vec![0; 4]]),
    ] {
        let error = Previews::default().render(&host, 1, Value::Null).err().expect("refused");
        assert_eq!(error.code, ErrorCode::IntegrityError, "{}", host.0);
    }
}
