//! 68i: the window's page previews through the RPC worker's raw-pixel path (pixels.rs) against a real host: `render.pixels`,
//! the held pixels, their decode, and the fallback to `render.get` and its PNG on a host without `render.pixels`.

use std::collections::BTreeSet;

use designllm_contracts::{Intent, Operation, Page, Size};
use eframe::egui::Pos2;

use super::test_support::{Host, Window};

/// A canvas of `pages` slides, each with one rectangle.
fn slides(host: &mut Host, pages: usize) -> String {
    let id = host.create("canvas", "픽셀 미리보기");
    let operations = (1..pages).map(|index| Operation::InsertPage { index: index as u32, page: Page::new(format!("extra-{index}"), Size::slide()) }).collect();
    let intents: BTreeSet<Intent> = [Intent::Content].into_iter().collect();
    if pages > 1 {
        host.apply(&id, operations, intents).expect("the pages are made");
    }
    id
}

fn methods(window: &Window) -> Vec<&str> {
    window.calls.iter().map(|(method, ..)| method.as_str()).collect()
}

#[test]
fn a_page_is_drawn_from_raw_pixels_and_no_stand_in_reaches_the_host() {
    let mut host = Host::start();
    let id = slides(&mut host, 1);
    let mut window = Window::open_with_pixel_previews(&mut host, &id, false);
    window.draw(&mut host, 1.0);
    let page = window.app.page_textures.get(&0).expect("page 0 has a texture");
    assert!(page.snapshot_id.len() > 0);
    let sent = methods(&window);
    assert!(sent.contains(&"render.pixels"), "{sent:?}");
    assert!(!sent.contains(&"render.get") && !sent.contains(&"asset.read"), "no PNG path: {sent:?}");
    // The snapshot of render.pixels answers the board's hit test.
    let rect = window.app.board_rect.expect("the board is drawn");
    window.click(&mut host, 1.0, Pos2::new(rect.center().x, rect.center().y), false);
    assert!(methods(&window).contains(&"render.hit_test"));
    assert!(window.message().is_none(), "{:?}", window.message());
}

#[test]
fn a_host_without_render_pixels_draws_the_page_from_render_get_and_its_png() {
    let mut host = Host::start();
    let id = slides(&mut host, 1);
    let mut window = Window::open_with_pixel_previews(&mut host, &id, true);
    window.draw(&mut host, 1.0);
    assert!(window.app.page_textures.contains_key(&0), "page 0 has a texture");
    let sent = methods(&window);
    assert!(sent.contains(&"render.pixels") && sent.contains(&"render.get") && sent.contains(&"asset.read"), "{sent:?}");
    assert!(window.message().is_none(), "{:?}", window.message());
}

#[test]
fn every_visible_page_of_a_zoomed_out_board_gets_its_raw_pixels() {
    let mut host = Host::start();
    let id = slides(&mut host, 8);
    let mut window = Window::open_with_pixel_previews(&mut host, &id, false);
    let view = window.app.view();
    view.zoom = 0.05;
    view.fitted = true;
    window.draw(&mut host, 1.0);
    window.draw(&mut host, 1.0);
    let drawn: Vec<u32> = (0..8).filter(|page| window.app.page_textures.contains_key(page)).collect();
    assert!(drawn.len() >= 4, "at least four pages are drawn: {drawn:?}");
    let sent = methods(&window);
    assert!(!sent.contains(&"asset.read") && !sent.contains(&"render.get"), "{sent:?}");
    assert!(window.message().is_none(), "{:?}", window.message());
}
