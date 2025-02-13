use std::any::Any;
use std::sync::Arc;

use glazier::kurbo::Size;
use glazier::{Application, IdleToken, Region, Scalable, WinHandler, WindowHandle};
use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache,
    TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use wgpu::{
    CommandEncoderDescriptor, CompositeAlphaMode, DeviceDescriptor, Instance, InstanceDescriptor,
    LoadOp, MultisampleState, Operations, PresentMode, RenderPassColorAttachment,
    RenderPassDescriptor, RequestAdapterOptions, SurfaceConfiguration, TextureFormat,
    TextureUsages, TextureViewDescriptor,
};

const WIDTH: usize = 2048;
const HEIGHT: usize = 1536;

struct InnerWindowState {
    window: Arc<WindowHandle>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: SurfaceConfiguration,

    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: glyphon::Viewport,
    atlas: glyphon::TextAtlas,
    text_renderer: glyphon::TextRenderer,
    text_buffer: glyphon::Buffer,
}

fn surface_size(handle: &WindowHandle) -> (u32, u32) {
    let scale = handle.get_scale().unwrap_or_default();
    let insets = handle.content_insets().to_px(scale);
    let mut size = handle.get_size().to_px(scale);
    size.width -= insets.x_value();
    size.height -= insets.y_value();
    (size.width as u32, size.height as u32)
}

impl InnerWindowState {
    fn create(window: WindowHandle) -> Self {
        let window = Arc::new(window);
        let physical_size = window.get_size();
        let scale_factor = window.get_scale().unwrap().y();

        // Set up surface
        let instance = Instance::new(InstanceDescriptor::default());
        let adapter =
            pollster::block_on(instance.request_adapter(&RequestAdapterOptions::default()))
                .unwrap();
        let (device, queue) =
            pollster::block_on(adapter.request_device(&DeviceDescriptor::default(), None)).unwrap();

        let surface = instance
            .create_surface(window.clone())
            .expect("Create surface");
        let swapchain_format = TextureFormat::Bgra8UnormSrgb;
        let surface_config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format: swapchain_format,
            width: physical_size.width as u32,
            height: physical_size.height as u32,
            present_mode: PresentMode::Fifo,
            alpha_mode: CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        // Set up text renderer
        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, swapchain_format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, MultisampleState::default(), None);
        let mut text_buffer = Buffer::new(&mut font_system, Metrics::new(14.0, 16.0));

        let physical_width = (physical_size.width / scale_factor) as f32 - 20.0;
        let physical_height = (physical_size.height / scale_factor) as f32 - 20.0;

        text_buffer.set_size(
            &mut font_system,
            Some(physical_width),
            Some(physical_height),
        );
        text_buffer.set_text(
            &mut font_system,
            &std::fs::read_to_string("samples/hello.txt").unwrap(),
            Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
        );
        text_buffer.shape_until_scroll(&mut font_system, false);

        Self {
            window,
            device,
            queue,
            surface,
            surface_config,
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            text_buffer,
        }
    }

    fn schedule_render(&self) {
        self.window.invalidate();
    }

    fn draw(&mut self) {
        let InnerWindowState {
            window,
            device,
            queue,
            surface,
            surface_config,
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            text_buffer,
            ..
        } = self;
        viewport.update(
            &queue,
            Resolution {
                width: surface_config.width,
                height: surface_config.height,
            },
        );

        text_renderer
            .prepare(
                device,
                queue,
                font_system,
                atlas,
                viewport,
                [TextArea {
                    buffer: text_buffer,
                    left: 20.0,
                    top: 20.0,
                    scale: window.get_scale().unwrap().y() as f32,
                    bounds: TextBounds {
                        left: 0,
                        top: 0,
                        right: surface_config.width as i32,
                        bottom: surface_config.height as i32,
                    },
                    default_color: Color::rgb(255, 255, 255),
                    custom_glyphs: &[],
                }],
                swash_cache,
            )
            .unwrap();

        let frame = surface.get_current_texture().unwrap();
        let view = frame.texture.create_view(&TextureViewDescriptor::default());
        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            text_renderer.render(&atlas, &viewport, &mut pass).unwrap();
        }

        queue.submit(Some(encoder.finish()));
        frame.present();

        atlas.trim();
    }
}

fn main() {
    let app = Application::new().unwrap();
    let window = glazier::WindowBuilder::new(app.clone())
        .resizable(true)
        .size((WIDTH as f64 / 2., HEIGHT as f64 / 2.).into())
        .handler(Box::new(WindowState::new()))
        .build()
        .unwrap();
    window.show();
    app.run(None);
}

struct WindowState {
    inner: Option<InnerWindowState>,
}

impl WindowState {
    fn new() -> Self {
        Self { inner: None }
    }
}

impl WinHandler for WindowState {
    fn connect(&mut self, handle: &WindowHandle) {
        let inner = InnerWindowState::create(handle.clone());
        inner.schedule_render();
        self.inner = Some(inner);
    }

    fn prepare_paint(&mut self) {}

    fn paint(&mut self, _: &Region) {
        let inner = self.inner.as_mut().unwrap();
        inner.draw();
        inner.schedule_render();
    }

    fn idle(&mut self, _: IdleToken) {}

    fn size(&mut self, _: Size) {
        let inner = self.inner.as_mut().unwrap();
        let size = surface_size(&inner.window);
        let InnerWindowState {
            window,
            device,
            surface,
            surface_config,
            font_system,
            text_buffer,
            ..
        } = inner;
        surface_config.width = size.0;
        surface_config.height = size.1;
        let scale_factor = window.get_scale().unwrap().y();
        surface.configure(&device, &surface_config);
        text_buffer.set_size(
            font_system,
            Some((size.0 as f64 / scale_factor) as f32 - 20.0),
            Some((size.1 as f64 / scale_factor) as f32 - 20.0),
        );
        inner.schedule_render();
    }

    fn request_close(&mut self) {
        self.inner.as_ref().unwrap().window.close();
    }

    fn destroy(&mut self) {
        Application::global().quit();
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}
