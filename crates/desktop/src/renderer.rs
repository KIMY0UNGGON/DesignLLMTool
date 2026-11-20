//! Which eframe renderer the windows of this process use (task 65). OpenGL (`egui_glow`) stays the default; where
//! the OpenGL driver cannot open, the launcher (`designllm_application`'s GL check) picks wgpu on DirectX 12, first on
//! a hardware adapter and then on WARP, the software adapter that Windows always has. Every `NativeOptions` of this
//! crate comes from `native_options`, so the probe child and the real window start the same renderer.

use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};

use eframe::{egui, egui_wgpu, wgpu};

/// Process-scoped override read by the launcher: `glow`, `wgpu` or `wgpu-warp`. Set, it is the only renderer tried.
pub const RENDERER_ENV: &str = "DESIGNLLM_RENDERER";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Renderer {
    Glow,
    /// wgpu on a DirectX 12 hardware adapter.
    Wgpu,
    /// wgpu on the DirectX 12 software adapter (WARP).
    WgpuWarp,
}

impl Renderer {
    /// The order the launcher tries when nothing is forced.
    pub const FALLBACK_ORDER: [Renderer; 3] = [Renderer::Glow, Renderer::Wgpu, Renderer::WgpuWarp];

    pub fn name(self) -> &'static str {
        match self {
            Renderer::Glow => "glow",
            Renderer::Wgpu => "wgpu",
            Renderer::WgpuWarp => "wgpu-warp",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::FALLBACK_ORDER.into_iter().find(|renderer| renderer.name() == value)
    }

    /// The graphics interface as the user knows it, for messages.
    pub fn api_name(self) -> &'static str {
        match self {
            Renderer::Glow => "OpenGL",
            Renderer::Wgpu => "DirectX 12",
            Renderer::WgpuWarp => "DirectX 12 WARP",
        }
    }

    /// `DESIGNLLM_RENDERER`: None when unset or empty, an error text for any other unknown value.
    pub fn forced() -> Result<Option<Self>, String> {
        let Some(value) = std::env::var_os(RENDERER_ENV).filter(|value| !value.is_empty()) else { return Ok(None) };
        let value = value.to_string_lossy();
        Self::parse(&value)
            .map(Some)
            .ok_or_else(|| format!("{RENDERER_ENV}={value} is not one of glow, wgpu, wgpu-warp"))
    }
}

static SELECTED: AtomicU8 = AtomicU8::new(0);

/// Sets the renderer of every window this process opens from now on.
pub fn select(renderer: Renderer) {
    let index = Renderer::FALLBACK_ORDER.iter().position(|item| *item == renderer).unwrap_or(0);
    SELECTED.store(index as u8, Ordering::Relaxed);
}

pub fn selected() -> Renderer {
    Renderer::FALLBACK_ORDER.get(usize::from(SELECTED.load(Ordering::Relaxed))).copied().unwrap_or(Renderer::Glow)
}

/// The window options for `viewport` on the selected renderer.
pub(crate) fn native_options(viewport: egui::ViewportBuilder) -> eframe::NativeOptions {
    options_for(selected(), viewport)
}

pub(crate) fn options_for(renderer: Renderer, viewport: egui::ViewportBuilder) -> eframe::NativeOptions {
    // With both eframe features on, eframe's own default is wgpu: the renderer is always named here.
    let mut options = eframe::NativeOptions { viewport, renderer: eframe::Renderer::Glow, ..Default::default() };
    if renderer != Renderer::Glow {
        options.renderer = eframe::Renderer::Wgpu;
        options.wgpu_options = wgpu_configuration(renderer == Renderer::WgpuWarp);
    }
    options
}

/// DirectX 12 only, with the FXC shader compiler that Windows ships (no dxcompiler.dll). `WGPU_*` environment
/// variables do not change the backend, the compiler or the adapter.
fn wgpu_configuration(software: bool) -> egui_wgpu::WgpuConfiguration {
    let setup = egui_wgpu::WgpuSetupCreateNew {
        instance_descriptor: wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            flags: wgpu::InstanceFlags::from_build_config(),
            backend_options: wgpu::BackendOptions::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        },
        native_adapter_selector: Some(Arc::new(move |adapters: &[wgpu::Adapter], surface: Option<&wgpu::Surface<'_>>| {
            pick_adapter(adapters, surface, software)
        })),
        ..Default::default()
    };
    egui_wgpu::WgpuConfiguration { wgpu_setup: setup.into(), ..Default::default() }
}

/// WARP is the adapter DXGI marks as software (`DeviceType::Cpu`). A hardware run takes a discrete GPU before an
/// integrated one, in enumeration order; it never takes WARP, so its failure is what moves the launch on to WARP.
fn pick_adapter(adapters: &[wgpu::Adapter], surface: Option<&wgpu::Surface<'_>>, software: bool) -> Result<wgpu::Adapter, String> {
    let rank = |device_type: wgpu::DeviceType| match device_type {
        wgpu::DeviceType::DiscreteGpu => 0,
        wgpu::DeviceType::IntegratedGpu => 1,
        _ => 2,
    };
    adapters
        .iter()
        .filter(|adapter| (adapter.get_info().device_type == wgpu::DeviceType::Cpu) == software)
        .filter(|adapter| surface.is_none_or(|surface| adapter.is_surface_supported(surface)))
        .min_by_key(|adapter| rank(adapter.get_info().device_type))
        .cloned()
        .ok_or_else(|| {
            let kind = if software { "software (WARP)" } else { "hardware" };
            format!("No DirectX 12 {kind} adapter can draw to the window")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_parse_back_and_unknown_values_are_refused() {
        for renderer in Renderer::FALLBACK_ORDER {
            assert_eq!(Renderer::parse(renderer.name()), Some(renderer));
        }
        assert_eq!(Renderer::parse("vulkan"), None);
        assert_eq!(Renderer::parse("WGPU"), None);
    }

    #[test]
    fn the_default_window_options_name_glow_and_the_fallbacks_name_wgpu_on_dx12_only() {
        let glow = options_for(Renderer::Glow, egui::ViewportBuilder::default());
        assert_eq!(glow.renderer, eframe::Renderer::Glow);
        for renderer in [Renderer::Wgpu, Renderer::WgpuWarp] {
            let options = options_for(renderer, egui::ViewportBuilder::default());
            assert_eq!(options.renderer, eframe::Renderer::Wgpu);
            let egui_wgpu::WgpuSetup::CreateNew(setup) = &options.wgpu_options.wgpu_setup else { panic!("a new wgpu setup") };
            assert_eq!(setup.instance_descriptor.backends, wgpu::Backends::DX12);
            assert!(matches!(setup.instance_descriptor.backend_options.dx12.shader_compiler, wgpu::Dx12Compiler::Fxc));
            assert!(setup.native_adapter_selector.is_some());
        }
    }

    #[test]
    fn the_selection_starts_at_glow_and_keeps_what_was_selected() {
        // One test owns the process-wide selection, so no other test sees it change.
        assert_eq!(selected(), Renderer::Glow);
        for renderer in Renderer::FALLBACK_ORDER.into_iter().rev() {
            select(renderer);
            assert_eq!(selected(), renderer);
        }
        assert_eq!(native_options(egui::ViewportBuilder::default()).renderer, eframe::Renderer::Glow);
    }
}
