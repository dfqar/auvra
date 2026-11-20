use still_core::{FramePlan, PointerInput, Quality, SceneDescriptor, SceneParameters};
use still_renderer::wgpu;

use crate::{FROST_DESCRIPTOR, FrostScene};

pub const SCENE_DESCRIPTORS: [SceneDescriptor; 1] = [FROST_DESCRIPTOR];

pub struct SceneOutput<'a> {
    pub source_key: u64,
    pub view: &'a wgpu::TextureView,
}

/// GPU-facing scene lifecycle. Scene implementations own all pipelines,
/// buffers, textures, simulation state, parameters, and preset definitions.
/// The runtime only schedules them and composites their linear output.
pub trait Scene {
    fn descriptor(&self) -> &'static SceneDescriptor;
    fn set_parameter(&mut self, stable_id: &str, value: f32) -> Option<f32>;
    fn set_parameters(&mut self, parameters: SceneParameters);
    fn set_quality(&mut self, quality: Quality);
    /// Optional transient media palette/envelope. Saved manual content is untouched.
    fn set_ambient_input(&mut self, _input: Option<still_core::AmbientFrame>) {}
    fn set_ambient_smoothing(&mut self, _settings: still_core::AmbientSmoothing) {}
    fn set_modulation(&mut self, _input: Option<still_core::SceneModulation>) {}
    fn prepare(&mut self, device: &wgpu::Device, viewport: [u32; 2]);
    fn encode<'a>(
        &'a mut self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        frame: FramePlan,
        pointer: PointerInput,
    ) -> Option<SceneOutput<'a>>;
    fn suspend(&mut self) {}
    fn resume(&mut self) {}
    fn release_gpu(&mut self);
}

pub fn descriptor(stable_id: &str) -> Option<&'static SceneDescriptor> {
    (stable_id == FROST_DESCRIPTOR.stable_id).then_some(&FROST_DESCRIPTOR)
}

pub fn create_scene(
    id: &str,
    device: &wgpu::Device,
    adapter: &wgpu::Adapter,
    quality: Quality,
    seed: u64,
) -> Option<Box<dyn Scene>> {
    descriptor(id)?;
    Some(Box::new(FrostScene::new(device, adapter, quality, seed)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_ids_and_parameter_ids_are_unique_and_namespaced() {
        for (scene_index, scene) in SCENE_DESCRIPTORS.iter().enumerate() {
            assert!(!scene.stable_id.is_empty());
            assert!(!scene.display_name.is_empty());
            assert!(scene.schema_version > 0);
            for other in SCENE_DESCRIPTORS.iter().skip(scene_index + 1) {
                assert_ne!(scene.stable_id, other.stable_id);
            }
            for (parameter_index, parameter) in scene.parameters.iter().enumerate() {
                assert!(
                    parameter
                        .stable_id
                        .starts_with(&format!("{}.", scene.stable_id))
                );
                for other in scene.parameters.iter().skip(parameter_index + 1) {
                    assert_ne!(parameter.stable_id, other.stable_id);
                }
            }
        }
    }

    #[test]
    fn frost_encodes_resizes_and_recreates_on_metal() {
        let renderer = match still_renderer::Renderer::new() {
            Ok(renderer) => renderer,
            Err(error) => {
                assert!(
                    std::env::var_os("STILL_REQUIRE_METAL_TESTS").is_none(),
                    "Metal required for this gate: {error}"
                );
                return;
            }
        };
        renderer
            .device
            .push_error_scope(wgpu::ErrorFilter::Validation);
        for descriptor in SCENE_DESCRIPTORS {
            for quality in [Quality::Eco, Quality::Balanced, Quality::High] {
                let mut scene = create_scene(
                    descriptor.stable_id,
                    &renderer.device,
                    &renderer.adapter,
                    quality,
                    7,
                )
                .unwrap();
                for viewport in [[640, 420], [1280, 840], [0, 0], [900, 560]] {
                    scene.prepare(&renderer.device, viewport);
                    let mut encoder = renderer.create_encoder();
                    let output = scene.encode(
                        &renderer.queue,
                        &mut encoder,
                        FramePlan {
                            animated: false,
                            ticks: 2,
                            fixed_dt: 1.0 / 120.0,
                            display_dt: 1.0 / 60.0,
                            interpolation: 0.25,
                        },
                        PointerInput::default(),
                    );
                    assert_eq!(output.is_some(), !viewport.contains(&0));
                    renderer.queue.submit(Some(encoder.finish()));
                    renderer.device.poll(wgpu::Maintain::Wait);
                }
                scene.suspend();
                scene.release_gpu();
                scene.resume();
                scene.prepare(&renderer.device, [800, 600]);
                let mut encoder = renderer.create_encoder();
                assert!(
                    scene
                        .encode(
                            &renderer.queue,
                            &mut encoder,
                            FramePlan::default(),
                            PointerInput::default()
                        )
                        .is_some()
                );
                renderer.queue.submit(Some(encoder.finish()));
                renderer.device.poll(wgpu::Maintain::Wait);
            }
        }
        assert!(pollster::block_on(renderer.device.pop_error_scope()).is_none());
    }
}
