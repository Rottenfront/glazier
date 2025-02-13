// use std::any::Any;
// use std::borrow::{Borrow, BorrowMut};

// use parley::layout::PositionedLayoutItem;
// use parley::{FontContext, Layout, LayoutContext};
// use tracing_subscriber::EnvFilter;
// use vello::util::{RenderContext, RenderSurface};
// use vello::{
//     kurbo::{Affine, PathEl, Point, Rect, Stroke},
//     peniko::{Brush, Color, Fill, Mix},
//     RenderParams, RendererOptions, Scene,
// };
// use vello::{AaSupport, DebugLayers, Renderer};

// use glazier::kurbo::Size;
// use glazier::{
//     Application, Cursor, FileDialogToken, FileInfo, IdleToken, KeyEvent, PointerEvent, Region,
//     Scalable, TimerToken, WinHandler, WindowHandle,
// };

// const WIDTH: usize = 2048;
// const HEIGHT: usize = 1536;

// fn main() {
//     tracing_subscriber::fmt()
//         .with_env_filter(EnvFilter::from_default_env())
//         .init();
//     let app = Application::new().unwrap();
//     let window = glazier::WindowBuilder::new(app.clone())
//         .size((WIDTH as f64 / 2., HEIGHT as f64 / 2.).into())
//         .handler(Box::new(WindowState::new()))
//         .build()
//         .unwrap();
//     window.show();
//     app.run(None);
// }

// struct WindowState {
//     handle: WindowHandle,
//     renderer: Option<Renderer>,
//     render: RenderContext,
//     surface: Option<RenderSurface<'static>>,
//     scene: Scene,
//     size: Size,
//     font_context: FontContext,
//     counter: u64,
// }

// impl WindowState {
//     pub fn new() -> Self {
//         let render = RenderContext::new();
//         Self {
//             handle: Default::default(),
//             surface: None,
//             renderer: None,
//             render,
//             scene: Default::default(),
//             font_context: FontContext::default(),
//             counter: 0,
//             size: Size::new(800.0, 600.0),
//         }
//     }

//     fn schedule_render(&self) {
//         self.handle.invalidate();
//     }

//     fn surface_size(&self) -> (u32, u32) {
//         let handle = &self.handle;
//         let scale = handle.get_scale().unwrap_or_default();
//         let insets = handle.content_insets().to_px(scale);
//         let mut size = handle.get_size().to_px(scale);
//         size.width -= insets.x_value();
//         size.height -= insets.y_value();
//         (size.width as u32, size.height as u32)
//     }

//     fn render(&mut self) {
//         let (width, height) = self.surface_size();
//         if self.surface.is_none() {
//             let handle = self.handle.clone();
//             self.surface = Some(
//                 pollster::block_on(self.render.create_surface(
//                     handle,
//                     width,
//                     height,
//                     wgpu::PresentMode::AutoVsync,
//                 ))
//                 .expect("failed to create surface"),
//             );
//         }

//         render_anim_frame(
//             &mut self.scene,
//             &mut self.font_context,
//             self.counter,
//             width as f32,
//         );
//         self.counter += 1;

//         if let Some(surface) = self.surface.as_mut() {
//             if surface.config.width != width || surface.config.height != height {
//                 self.render.resize_surface(surface, width, height);
//             }
//             let surface_texture = surface.surface.get_current_texture().unwrap();
//             let dev_id = surface.dev_id;
//             let device = &self.render.devices[dev_id].device;
//             let queue = &self.render.devices[dev_id].queue;
//             let renderer_options = RendererOptions {
//                 surface_format: Some(surface.format),
//                 use_cpu: false,
//                 antialiasing_support: AaSupport {
//                     area: true,
//                     msaa8: false,
//                     msaa16: false,
//                 },
//                 num_init_threads: Default::default(),
//             };
//             let render_params = RenderParams {
//                 base_color: Color::BLACK,
//                 width,
//                 height,
//                 antialiasing_method: vello::AaConfig::Area,
//                 debug: DebugLayers::all(),
//             };
//             self.renderer
//                 .get_or_insert_with(|| Renderer::new(device, renderer_options).unwrap())
//                 .render_to_surface(device, queue, &self.scene, &surface_texture, &render_params)
//                 .unwrap();
//             surface_texture.present();
//         }
//     }
// }

// impl WinHandler for WindowState {
//     fn connect(&mut self, handle: &WindowHandle) {
//         self.handle = handle.clone();
//         self.schedule_render();
//     }

//     fn prepare_paint(&mut self) {}

//     fn paint(&mut self, _: &Region) {
//         self.render();
//         self.schedule_render();
//     }

//     fn idle(&mut self, _: IdleToken) {}

//     fn command(&mut self, _id: u32) {}

//     fn open_file(&mut self, _token: FileDialogToken, file_info: Option<FileInfo>) {
//         println!("open file result: {file_info:?}");
//     }

//     fn save_as(&mut self, _token: FileDialogToken, file: Option<FileInfo>) {
//         println!("save file result: {file:?}");
//     }

//     fn key_down(&mut self, event: &KeyEvent) -> bool {
//         println!("keydown: {event:?}");
//         false
//     }

//     fn key_up(&mut self, event: &KeyEvent) {
//         println!("keyup: {event:?}");
//     }

//     fn wheel(&mut self, event: &PointerEvent) {
//         println!("wheel {event:?}");
//     }

//     fn pointer_move(&mut self, _event: &PointerEvent) {
//         self.handle.set_cursor(&Cursor::Arrow);
//         //println!("pointer_move {event:?}");
//     }

//     fn pointer_down(&mut self, event: &PointerEvent) {
//         println!("pointer_down {event:?}");
//     }

//     fn pointer_up(&mut self, event: &PointerEvent) {
//         println!("pointer_up {event:?}");
//     }

//     fn timer(&mut self, id: TimerToken) {
//         println!("timer fired: {id:?}");
//     }

//     fn size(&mut self, size: Size) {
//         self.size = size;
//     }

//     fn got_focus(&mut self) {
//         println!("Got focus");
//     }

//     fn lost_focus(&mut self) {
//         println!("Lost focus");
//     }

//     fn request_close(&mut self) {
//         self.handle.close();
//     }

//     fn destroy(&mut self) {
//         Application::global().quit();
//     }

//     fn as_any(&mut self) -> &mut dyn Any {
//         self
//     }
// }

// pub fn render_anim_frame(scene: &mut Scene, fcx: &mut FontContext, i: u64, width: f32) {
//     scene.reset();
//     let rect = Rect::from_origin_size(Point::new(0.0, 0.0), (1000.0, 1000.0));
//     scene.fill(
//         Fill::NonZero,
//         Affine::IDENTITY,
//         &Brush::Solid(Color::rgb8(128, 128, 128)),
//         None,
//         &rect,
//     );

//     let scale = (i as f64 * 0.01).sin() * 0.5 + 1.5;
//     let transform = Affine::translate((INSET as f64, INSET as f64));

//     let mut layout = Layout::<Color>::new();
//     let mut layout_cx = LayoutContext::new();

//     let mut builder = layout_cx.ranged_builder(fcx, LOREM, scale as f32);
//     builder.push_default(&parley::style::StyleProperty::FontSize(32.0));
//     builder.push_default(&parley::style::StyleProperty::LineHeight(1.2));
//     builder.push_default(&parley::style::StyleProperty::FontStack(
//         parley::style::FontStack::Source("system-ui"),
//     ));
//     builder.build_into(&mut layout);
//     layout.break_all_lines(Some(width - INSET * 2.0));
//     layout.align(Some(width - INSET * 2.0), parley::layout::Alignment::Start);

//     for line in layout.lines() {
//         for item in line.items() {
//             let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
//                 continue;
//             };
//             let mut x = glyph_run.offset();
//             let y = glyph_run.baseline();
//             let run = glyph_run.run();
//             let font = run.font();
//             let font_size = run.font_size();
//             let synthesis = run.synthesis();
//             let glyph_xform = synthesis
//                 .skew()
//                 .map(|angle| Affine::skew(angle.to_radians().tan() as f64, 0.0));
//             let coords = run
//                 .normalized_coords()
//                 .iter()
//                 .map(|coord| vello::skrifa::instance::NormalizedCoord::from_bits(*coord))
//                 .collect::<Vec<_>>();
//             scene
//                 .draw_glyphs(font)
//                 .brush(Color::WHITE)
//                 .hint(true)
//                 .transform(transform)
//                 .glyph_transform(glyph_xform)
//                 .font_size(font_size)
//                 .normalized_coords(&coords)
//                 .draw(
//                     Fill::NonZero,
//                     glyph_run.glyphs().map(|glyph| {
//                         let gx = x + glyph.x;
//                         let gy = y - glyph.y;
//                         x += glyph.advance;
//                         vello::glyph::Glyph {
//                             id: glyph.id as _,
//                             x: gx,
//                             y: gy,
//                         }
//                     }),
//                 );
//         }
//     }

//     let th = (std::f64::consts::PI / 180.0) * (i as f64);
//     let center = Point::new(500.0, 500.0);
//     let mut p1 = center;
//     p1.x += 400.0 * th.cos();
//     p1.y += 400.0 * th.sin();
//     scene.stroke(
//         &Stroke::new(5.0),
//         Affine::IDENTITY,
//         &Brush::Solid(Color::rgb8(128, 0, 0)),
//         None,
//         &[PathEl::MoveTo(center), PathEl::LineTo(p1)],
//     );
//     scene.fill(
//         Fill::NonZero,
//         Affine::translate((150.0, 150.0)) * Affine::scale(0.2),
//         Color::RED,
//         None,
//         &rect,
//     );
//     let alpha = (i as f64 * 0.03).sin() as f32 * 0.5 + 0.5;
//     scene.push_layer(Mix::Normal, alpha, Affine::IDENTITY, &rect);
//     scene.fill(
//         Fill::NonZero,
//         Affine::translate((100.0, 100.0)) * Affine::scale(0.2),
//         Color::BLUE,
//         None,
//         &rect,
//     );
//     scene.fill(
//         Fill::NonZero,
//         Affine::translate((200.0, 200.0)) * Affine::scale(0.2),
//         Color::GREEN,
//         None,
//         &rect,
//     );
//     scene.pop_layer();
// }

// pub const LOREM: &str = r" Lorem ipsum dolor sit amet, consectetur adipiscing elit. Morbi cursus mi sed euismod euismod. Orci varius natoque penatibus et magnis dis parturient montes, nascetur ridiculus mus. Nullam placerat efficitur tellus at semper. Morbi ac risus magna. Donec ut cursus ex. Etiam quis posuere tellus. Mauris posuere dui et turpis mollis, vitae luctus tellus consectetur. Lorem ipsum dolor sit amet, consectetur adipiscing elit. Curabitur eu facilisis nisl.

// Phasellus in viverra dolor, vitae facilisis est. Maecenas malesuada massa vel ultricies feugiat. Vivamus venenatis et gהתעשייה בנושא האינטרנטa nibh nec pharetra. Phasellus vestibulum elit enim, nec scelerisque orci faucibus id. Vivamus consequat purus sit amet orci egestas, non iaculis massa porttitor. Vestibulum ut eros leo. In fermentum convallis magna in finibus. Donec justo leo, maximus ac laoreet id, volutpat ut elit. Mauris sed leo non neque laoreet faucibus. Aliquam orci arcu, faucibus in molestie eget, ornare non dui. Donec volutpat nulla in fringilla elementum. Aliquam vitae ante egestas ligula tempus vestibulum sit amet sed ante. ";

// const INSET: f32 = 32.0;

fn main() {}
