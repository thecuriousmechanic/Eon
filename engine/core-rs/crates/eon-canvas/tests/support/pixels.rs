use std::{
    sync::{Arc, mpsc},
    time::Duration,
};

use eframe::egui_wgpu::{Renderer, RendererOptions, ScreenDescriptor, capture::CaptureState};
use egui::ColorImage;

/// Read back a complete frame (including its initial font atlas).
pub fn render(
    ctx: &egui::Context,
    mut output: egui::FullOutput,
    size: [u32; 2],
) -> Arc<ColorImage> {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = desc.backends.with_env();
    let instance = wgpu::Instance::new(desc);
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let extent = wgpu::Extent3d {
        width: size[0],
        height: size[1],
        depth_or_array_layers: 1,
    };
    let mut capture = CaptureState::new(&device, extent, format);
    let mut renderer = Renderer::new(&device, format, RendererOptions::PREDICTABLE);
    for (id, deltas) in output.textures_delta.set.drain() {
        for delta in deltas {
            renderer.update_texture(&device, &queue, id, &delta);
        }
    }
    let jobs = ctx.tessellate(output.shapes, output.pixels_per_point);
    let screen = ScreenDescriptor {
        size_in_pixels: size,
        pixels_per_point: output.pixels_per_point,
    };
    let mut encoder = device.create_command_encoder(&Default::default());
    let extra = renderer.update_buffers(&device, &queue, &mut encoder, &jobs, &screen);
    {
        let view = capture.texture.create_view(&Default::default());
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: Default::default(),
                depth_slice: None,
            })],
            ..Default::default()
        });
        renderer.render(&mut pass.forget_lifetime(), &jobs, &screen);
    }
    let buffer = capture.copy_textures(&device, None, &mut encoder);
    queue.submit(extra.into_iter().chain([encoder.finish()]));
    for id in output.textures_delta.free.drain() {
        renderer.free_texture(&id);
    }
    let (tx, rx) = mpsc::channel();
    capture.read_screen_rgba(
        buffer,
        vec![egui::ScreenshotCallback::new(move |image| {
            tx.send(image).unwrap()
        })],
    );
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    rx.recv_timeout(Duration::from_secs(5)).unwrap()
}
