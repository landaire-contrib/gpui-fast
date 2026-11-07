// Modified by Longbridge for gpui-fast.
use gpui::{
    AppContext as _, Bounds, Context, DevicePixels, DynamicTexture, IntoElement,
    ParentElement as _, PlatformHeadlessRenderer as _, Render, Styled as _, Window, canvas, div,
    point, px, size,
};
use parking_lot::Mutex;

use super::tests::{draw_other, pixel};
use crate::{WgpuContext, WgpuHeadlessRenderer};

/// The published context is process-wide; tests that publish one take turns.
static PUBLISHED: Mutex<()> = Mutex::new(());

const RED: [u8; 4] = [0, 0, 255, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [255, 0, 0, 255];

/// Paints its texture over the whole 64 px view, on white.
struct Showcase(Option<DynamicTexture>);

impl Render for Showcase {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let texture = self.0;
        div().size(px(64.)).bg(gpui::white()).child(
            canvas(
                |_, _, _| (),
                move |bounds, (), window, _| {
                    if let Some(texture) = texture {
                        window
                            .paint_dynamic_texture(bounds, texture)
                            .expect("CPU texture paint");
                    }
                },
            )
            .size(px(64.)),
        )
    }
}

/// `width` x `height` BGRA8 pixels of `color`, in rows of `stride` bytes.
fn rows(width: usize, height: usize, stride: usize, color: [u8; 4]) -> Vec<u8> {
    let mut bytes = vec![0xAA; stride * (height - 1) + width * 4];
    for row in 0..height {
        for column in 0..width {
            let start = row * stride + column * 4;
            bytes[start..start + 4].copy_from_slice(&color);
        }
    }
    bytes
}

fn render(
    renderer: &mut WgpuHeadlessRenderer,
    window: &mut Window,
    cx: &mut gpui::App,
) -> (image::RgbaImage, f32) {
    window.draw(cx).clear(cx);
    let scale = window.scale_factor();
    let side = DevicePixels((64. * scale) as i32);
    let image = renderer
        .render_scene_to_image(window.painted_scene(), size(side, side))
        .unwrap();
    (image, scale)
}

#[gpui::test]
fn cpu_textures_are_created_updated_painted_and_removed(cx: &mut gpui::TestAppContext) {
    let _published = PUBLISHED.lock();
    let (context, _) = WgpuContext::new_headless().expect("GPU required");
    crate::fast::app_context::publish(&context);
    let mut renderer = WgpuHeadlessRenderer::new().unwrap();
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|_| Showcase(None)))
            .unwrap()
    });

    let texture = window
        .update(cx, |showcase, window, cx| {
            let texture = window
                .create_dynamic_texture(
                    size(DevicePixels(16), DevicePixels(16)),
                    &rows(16, 16, 64, RED),
                    64,
                )
                .unwrap();
            showcase.0 = Some(texture);
            cx.notify();
            texture
        })
        .unwrap();
    let view = context
        .fast_dynamic_textures
        .view(texture.id())
        .expect("the texture is registered with the published context");
    assert_eq!(
        view.texture().usage(),
        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST
    );
    window
        .update(cx, |_, window, cx| {
            let (image, scale) = render(&mut renderer, window, cx);
            pixel(&image, scale, 16., 32., [255, 0, 0, 255]);
            pixel(&image, scale, 48., 32., [255, 0, 0, 255]);
        })
        .unwrap();

    // The right half, from strided rows with padding.
    let stride = 8 * 4 + 12;
    window
        .update(cx, |_, window, _| {
            window
                .update_dynamic_texture(
                    texture,
                    Bounds {
                        origin: point(DevicePixels(8), DevicePixels(0)),
                        size: size(DevicePixels(8), DevicePixels(16)),
                    },
                    &rows(8, 16, stride, BLUE),
                    stride as u32,
                )
                .unwrap();
        })
        .unwrap();
    window
        .update(cx, |_, window, cx| {
            let (image, scale) = render(&mut renderer, window, cx);
            pixel(&image, scale, 16., 32., [255, 0, 0, 255]);
            pixel(&image, scale, 48., 32., [0, 0, 255, 255]);
        })
        .unwrap();

    window
        .update(cx, |_, window, _| {
            window.remove_dynamic_texture(texture);
            assert!(
                window
                    .update_dynamic_texture(
                        texture,
                        Bounds {
                            origin: point(DevicePixels(0), DevicePixels(0)),
                            size: size(DevicePixels(1), DevicePixels(1)),
                        },
                        &RED,
                        4,
                    )
                    .is_err()
            );
        })
        .unwrap();
    assert!(context.fast_dynamic_textures.view(texture.id()).is_none());
    window
        .update(cx, |_, window, _| {
            // The scene painted before the removal still holds the sprite.
            let scale = window.scale_factor();
            let side = DevicePixels((64. * scale) as i32);
            let image = renderer
                .render_scene_to_image(window.painted_scene(), size(side, side))
                .unwrap();
            pixel(&image, scale, 16., 32., [255, 255, 255, 255]);
        })
        .unwrap();
}

#[gpui::test]
fn cpu_textures_keep_their_contents_on_a_newly_published_context(cx: &mut gpui::TestAppContext) {
    let _published = PUBLISHED.lock();
    let (context, _) = WgpuContext::new_headless().expect("GPU required");
    crate::fast::app_context::publish(&context);
    let first = crate::app_context().expect("published").generation;
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|_| Showcase(None)))
            .unwrap()
    });
    let texture = window
        .update(cx, |showcase, window, cx| {
            let texture = window
                .create_dynamic_texture(
                    size(DevicePixels(16), DevicePixels(16)),
                    &rows(16, 16, 64, GREEN),
                    64,
                )
                .unwrap();
            showcase.0 = Some(texture);
            cx.notify();
            texture
        })
        .unwrap();

    static RETIRED: Mutex<Vec<crate::WgpuContextRetired>> = Mutex::new(Vec::new());
    crate::on_device_lost(|retired| RETIRED.lock().push(*retired));
    let (other, other_format) = WgpuContext::create_headless().expect("second GPU device");
    crate::fast::app_context::publish(&other);
    assert!(
        RETIRED
            .lock()
            .iter()
            .any(|retired| retired.generation == first
                && retired.reason == crate::WgpuContextRetirement::Replaced),
        "the replaced context is retired"
    );
    let published = crate::app_context().expect("published");
    assert!(published.generation > first);
    assert_eq!(published.context.device.as_ref(), other.device.as_ref());
    assert!(context.fast_dynamic_textures.view(texture.id()).is_none());
    assert!(other.fast_dynamic_textures.view(texture.id()).is_some());

    let draw = |cx: &mut gpui::TestAppContext| {
        window
            .update(cx, |_, window, cx| {
                window.draw(cx).clear(cx);
                let scale = window.scale_factor();
                let image = draw_other(
                    &other,
                    other_format,
                    window.painted_scene(),
                    (64. * scale) as u32,
                );
                (image, scale)
            })
            .unwrap()
    };
    // The new device's texture holds what the app last wrote.
    let (image, scale) = draw(cx);
    pixel(&image, scale, 16., 32., [0, 255, 0, 255]);
    pixel(&image, scale, 48., 32., [0, 255, 0, 255]);

    window
        .update(cx, |_, window, _| {
            window
                .update_dynamic_texture(
                    texture,
                    Bounds {
                        origin: point(DevicePixels(0), DevicePixels(0)),
                        size: size(DevicePixels(8), DevicePixels(16)),
                    },
                    &rows(8, 16, 32, BLUE),
                    32,
                )
                .unwrap();
        })
        .unwrap();
    let (image, scale) = draw(cx);
    pixel(&image, scale, 16., 32., [0, 0, 255, 255]);
    pixel(&image, scale, 48., 32., [0, 255, 0, 255]);
    window
        .update(cx, |_, window, _| window.remove_dynamic_texture(texture))
        .unwrap();
    assert!(other.fast_dynamic_textures.view(texture.id()).is_none());
    crate::fast::app_context::publish(&context);
}

/// Writes between frames stay in the CPU copy, merged into one region, so a
/// producer streaming into a window that does not draw stages nothing on the
/// queue; the next frame uploads them once.
#[gpui::test]
fn cpu_writes_coalesce_until_a_frame_uploads_them(cx: &mut gpui::TestAppContext) {
    let _published = PUBLISHED.lock();
    // A device of its own: other tests draw frames on the shared headless
    // device, and any frame on the published device uploads pending writes.
    let (context, format) = WgpuContext::create_headless().expect("GPU required");
    crate::fast::app_context::publish(&context);
    let render = |window: &mut gpui::Window, cx: &mut gpui::App| {
        window.draw(cx).clear(cx);
        let scale = window.scale_factor();
        let image = draw_other(
            &context,
            format,
            window.painted_scene(),
            (64. * scale) as u32,
        );
        (image, scale)
    };
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|_| Showcase(None)))
            .unwrap()
    });
    let texture = window
        .update(cx, |showcase, window, cx| {
            let texture = window
                .create_dynamic_texture(
                    size(DevicePixels(16), DevicePixels(16)),
                    &rows(16, 16, 64, RED),
                    64,
                )
                .unwrap();
            showcase.0 = Some(texture);
            cx.notify();
            texture
        })
        .unwrap();
    let pending = || super::upload::pending_region(texture.id());
    window
        .update(cx, |_, window, cx| {
            render(window, cx);
        })
        .unwrap();
    assert_eq!(pending(), None, "the frame uploaded the initial contents");

    window
        .update(cx, |_, window, _| {
            for frame in 0..1000 {
                let color = if frame % 2 == 0 { GREEN } else { BLUE };
                for (x, y) in [(0, 0), (12, 12)] {
                    window
                        .update_dynamic_texture(
                            texture,
                            Bounds {
                                origin: point(DevicePixels(x), DevicePixels(y)),
                                size: size(DevicePixels(4), DevicePixels(4)),
                            },
                            &rows(4, 4, 16, color),
                            16,
                        )
                        .unwrap();
                }
            }
        })
        .unwrap();
    assert_eq!(
        pending(),
        Some(Bounds {
            origin: point(DevicePixels(0), DevicePixels(0)),
            size: size(DevicePixels(16), DevicePixels(16)),
        }),
        "every write merged into one region"
    );

    window
        .update(cx, |_, window, cx| {
            let (image, scale) = render(window, cx);
            // The last write of each corner wins; the middle keeps red.
            pixel(&image, scale, 2., 2., [0, 0, 255, 255]);
            pixel(&image, scale, 62., 62., [0, 0, 255, 255]);
            pixel(&image, scale, 32., 32., [255, 0, 0, 255]);
            window.remove_dynamic_texture(texture);
        })
        .unwrap();
    assert_eq!(pending(), None);
    let (shared, _) = WgpuContext::new_headless().expect("GPU required");
    crate::fast::app_context::publish(&shared);
}

/// Releasing the context tells the application, refuses new CPU textures,
/// keeps the contents written meanwhile, and the next published context
/// shows them under a later generation.
#[gpui::test]
fn a_released_context_is_retired_and_its_textures_return_with_the_next(
    cx: &mut gpui::TestAppContext,
) {
    use std::sync::atomic::{AtomicU64, Ordering};

    let _published = PUBLISHED.lock();
    let (context, _) = WgpuContext::new_headless().expect("GPU required");
    crate::fast::app_context::publish(&context);
    let first = crate::app_context().expect("published").generation;
    static RELEASED: Mutex<Vec<crate::WgpuContextRetired>> = Mutex::new(Vec::new());
    static CALLS: AtomicU64 = AtomicU64::new(0);
    crate::on_device_lost(|retired| {
        CALLS.fetch_add(1, Ordering::Relaxed);
        RELEASED.lock().push(*retired);
    });

    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|_| Showcase(None)))
            .unwrap()
    });
    let texture = window
        .update(cx, |showcase, window, cx| {
            let texture = window
                .create_dynamic_texture(
                    size(DevicePixels(16), DevicePixels(16)),
                    &rows(16, 16, 64, RED),
                    64,
                )
                .unwrap();
            showcase.0 = Some(texture);
            cx.notify();
            texture
        })
        .unwrap();

    // Not lost: nothing is retired.
    crate::fast::app_context::device_lost();
    assert_eq!(CALLS.load(Ordering::Relaxed), 0);

    crate::release_app_context();
    crate::release_app_context();
    assert_eq!(CALLS.load(Ordering::Relaxed), 1, "retired once");
    let retired = RELEASED.lock()[0];
    assert_eq!(retired.generation, first);
    assert_eq!(retired.reason, crate::WgpuContextRetirement::Released);
    assert!(crate::app_context().is_none());
    assert!(context.fast_dynamic_textures.view(texture.id()).is_none());

    window
        .update(cx, |_, window, _| {
            assert!(
                window
                    .create_dynamic_texture(
                        size(DevicePixels(4), DevicePixels(4)),
                        &rows(4, 4, 16, RED),
                        16
                    )
                    .is_err(),
                "no texture is created without a context"
            );
            window
                .update_dynamic_texture(
                    texture,
                    Bounds {
                        origin: point(DevicePixels(0), DevicePixels(0)),
                        size: size(DevicePixels(16), DevicePixels(16)),
                    },
                    &rows(16, 16, 64, BLUE),
                    64,
                )
                .expect("writes are kept while released");
        })
        .unwrap();

    crate::fast::app_context::publish(&context);
    assert!(crate::app_context().expect("published").generation > first);
    let mut renderer = WgpuHeadlessRenderer::new().unwrap();
    window
        .update(cx, |_, window, cx| {
            let (image, scale) = render(&mut renderer, window, cx);
            pixel(&image, scale, 32., 32., [0, 0, 255, 255]);
            window.remove_dynamic_texture(texture);
        })
        .unwrap();
}
