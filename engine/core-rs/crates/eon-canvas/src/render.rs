//! Headless `wgpu` renderer for layered graphs. Node boxes and edge polylines are drawn into an RGBA8 image, which the egui
//! shell can show as a texture later (docs/implementation/05, section 5.2). Runs on any Vulkan, Metal, or DX12 adapter,
//! including the software Vulkan (lavapipe) that CPU-only hosts provide.

use std::sync::mpsc;

use wgpu::util::DeviceExt;

use crate::Layout;

pub const NODE_WIDTH: f32 = 120.0;
pub const NODE_HEIGHT: f32 = 40.0;
const COLUMN_WIDTH: f32 = 160.0;
const ROW_HEIGHT: f32 = 100.0;
const MARGIN: f32 = 40.0;
const EDGE_THICKNESS: f32 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Style {
    pub background: [f32; 4],
    pub node: [f32; 4],
    pub edge: [f32; 4],
}

impl Default for Style {
    fn default() -> Self {
        Self {
            background: [0.05, 0.05, 0.08, 1.0],
            node: [0.2, 0.5, 0.9, 1.0],
            edge: [0.9, 0.7, 0.2, 1.0],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Image {
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let offset = ((y * self.width + x) * 4) as usize;
        [
            self.rgba[offset],
            self.rgba[offset + 1],
            self.rgba[offset + 2],
            self.rgba[offset + 3],
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    NoAdapter(String),
    Device(String),
    Readback(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::NoAdapter(reason) => write!(f, "no GPU adapter: {reason}"),
            RenderError::Device(reason) => write!(f, "device error: {reason}"),
            RenderError::Readback(reason) => write!(f, "readback failed: {reason}"),
        }
    }
}

impl std::error::Error for RenderError {}

const SHADER: &str = r#"
struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(@location(0) position: vec2<f32>, @location(1) color: vec4<f32>) -> VertexOut {
    var out: VertexOut;
    out.position = vec4<f32>(position, 0.0, 1.0);
    out.color = color;
    return out;
}

@fragment
fn fs_main(input: VertexOut) -> @location(0) vec4<f32> {
    return input.color;
}
"#;

/// Pixel size of the image that `render` produces for `layout`.
pub fn image_size(layout: &Layout) -> (u32, u32) {
    let columns = layout.slot.iter().map(|s| s + 1).max().unwrap_or(1);
    let rows = layout.rank.iter().map(|r| r + 1).max().unwrap_or(1);
    let width = 2.0 * MARGIN + columns as f32 * COLUMN_WIDTH;
    let height = 2.0 * MARGIN + rows as f32 * ROW_HEIGHT;
    (width as u32, height as u32)
}

pub fn node_centre(rank: usize, slot: usize) -> [f32; 2] {
    [
        MARGIN + slot as f32 * COLUMN_WIDTH + COLUMN_WIDTH / 2.0,
        MARGIN + rank as f32 * ROW_HEIGHT + ROW_HEIGHT / 2.0,
    ]
}

fn to_ndc(p: [f32; 2], (width, height): (u32, u32)) -> [f32; 2] {
    [p[0] / width as f32 * 2.0 - 1.0, 1.0 - p[1] / height as f32 * 2.0]
}

fn push_vertex(vertices: &mut Vec<f32>, p: [f32; 2], color: [f32; 4], size: (u32, u32)) {
    vertices.extend_from_slice(&to_ndc(p, size));
    vertices.extend_from_slice(&color);
}

fn push_box(vertices: &mut Vec<f32>, a: [f32; 2], b: [f32; 2], color: [f32; 4], size: (u32, u32)) {
    for p in [a, [b[0], a[1]], b, a, b, [a[0], b[1]]] {
        push_vertex(vertices, p, color, size);
    }
}

/// Triangles for the boxes and edges, in pixel coordinates, each vertex as x, y, r, g, b, a.
fn geometry(layout: &Layout, style: &Style, size: (u32, u32)) -> Vec<f32> {
    let mut vertices = Vec::new();

    for path in &layout.edge_paths {
        for pair in path.windows(2) {
            let a = node_centre(pair[0].0, pair[0].1);
            let b = node_centre(pair[1].0, pair[1].1);
            let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
            let length = (dx * dx + dy * dy).sqrt().max(f32::EPSILON);
            let (nx, ny) = (-dy / length * EDGE_THICKNESS / 2.0, dx / length * EDGE_THICKNESS / 2.0);
            let corners = [
                [a[0] + nx, a[1] + ny],
                [a[0] - nx, a[1] - ny],
                [b[0] - nx, b[1] - ny],
                [b[0] + nx, b[1] + ny],
            ];
            for index in [0, 1, 2, 0, 2, 3] {
                push_vertex(&mut vertices, corners[index], style.edge, size);
            }
        }
    }

    for (&rank, &slot) in layout.rank.iter().zip(&layout.slot) {
        let [cx, cy] = node_centre(rank, slot);
        push_box(
            &mut vertices,
            [cx - NODE_WIDTH / 2.0, cy - NODE_HEIGHT / 2.0],
            [cx + NODE_WIDTH / 2.0, cy + NODE_HEIGHT / 2.0],
            style.node,
            size,
        );
    }
    vertices
}


fn align_to(value: u32, alignment: u32) -> u32 {
    value.div_ceil(alignment) * alignment
}

/// Renders `layout` headlessly and returns the RGBA8 pixels.
pub fn render(layout: &Layout, style: &Style) -> Result<Image, RenderError> {
    let (width, height) = image_size(layout);
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = descriptor.backends.with_env();
    let instance = wgpu::Instance::new(descriptor);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        .map_err(|e| RenderError::NoAdapter(e.to_string()))?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
        .map_err(|e| RenderError::Device(e.to_string()))?;

    let format = wgpu::TextureFormat::Rgba8Unorm;
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("eon-canvas target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("eon-canvas shader"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor::default());
    let vertex_layout = wgpu::VertexBufferLayout {
        array_stride: 24,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &[
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 0,
                shader_location: 0,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 8,
                shader_location: 1,
            },
        ],
    };
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("eon-canvas pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            buffers: &[Some(vertex_layout)],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        multiview_mask: None,
        cache: None,
    });

    let vertices = geometry(layout, style, (width, height));
    let vertex_bytes: Vec<u8> = vertices.iter().flat_map(|f| f.to_le_bytes()).collect();
    let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("eon-canvas vertices"),
        contents: &vertex_bytes,
        usage: wgpu::BufferUsages::VERTEX,
    });
    let vertex_count = (vertices.len() / 6) as u32;

    let row_bytes = align_to(width * 4, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("eon-canvas readback"),
        size: (row_bytes * height) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("eon-canvas pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: style.background[0] as f64,
                        g: style.background[1] as f64,
                        b: style.background[2] as f64,
                        a: style.background[3] as f64,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_vertex_buffer(0, vertex_buffer.slice(..));
        pass.draw(0..vertex_count, 0..1);
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row_bytes),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(Some(encoder.finish()));

    let (sender, receiver) = mpsc::channel();
    readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    receiver
        .recv()
        .map_err(|e| RenderError::Readback(e.to_string()))?
        .map_err(|e| RenderError::Readback(e.to_string()))?;

    let mapped = readback
        .slice(..)
        .get_mapped_range()
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for row in mapped.chunks(row_bytes as usize).take(height as usize) {
        rgba.extend_from_slice(&row[..(width * 4) as usize]);
    }
    drop(mapped);
    readback.unmap();

    Ok(Image { width, height, rgba })
}
