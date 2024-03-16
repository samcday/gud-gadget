use drm::buffer::Buffer;
use drm::control::Device;
use gud_gadget::{DisplayMode, Event};
use std::env::args;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use usb_gadget::function::custom::{Custom, Interface};
use usb_gadget::{default_udc, Class, Config, Gadget, Strings};

#[derive(Debug)]
/// A simple wrapper for a device node.
pub struct Card(std::fs::File);

/// Implementing `AsFd` is a prerequisite to implementing the traits found
/// in this crate. Here, we are just calling `as_fd()` on the inner File.
impl std::os::unix::io::AsFd for Card {
    fn as_fd(&self) -> std::os::unix::io::BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// With `AsFd` implemented, we can now implement `drm::Device`.
impl drm::Device for Card {}

impl Device for Card {}

/// Simple helper methods for opening a `Card`.
impl Card {
    pub fn open(path: &str) -> Self {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        options.write(true);
        Card(options.open(path).unwrap())
    }

    pub fn open_global() -> Self {
        Self::open("/dev/dri/card0")
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let card_path = args()
        .skip(1)
        .next()
        .expect("specify full path to /dev/dri/cardN as program argument");
    let card = Card::open(&card_path);
    let udc = default_udc().expect("no UDC found");

    let resources = card.resource_handles().expect("load drm resources failed");

    let connector = resources
        .connectors()
        .iter()
        .map(|h| {
            card.get_connector(*h, false)
                .expect("get drm connector failed")
        })
        .find(|c| c.state() == drm::control::connector::State::Connected)
        .expect("no connected connectors found");

    let crtc = resources
        .crtcs()
        .iter()
        .flat_map(|crtc| card.get_crtc(*crtc))
        .next()
        .expect("no crtc found");

    let mut min_width = u32::MAX;
    let mut min_height = u32::MAX;
    let mut max_width = 0;
    let mut max_height = 0;
    for mode in connector.modes() {
        let (width, height) = mode.size();
        let width = width as u32;
        let height = height as u32;
        if width < min_width {
            min_width = width
        }
        if width > max_width {
            max_width = width
        }
        if height < min_height {
            min_height = height
        }
        if height > max_height {
            max_height = height
        }
    }

    usb_gadget::remove_all().expect("UDC init failed");

    let (mut gud_data, gud_data_ep) = gud_gadget::PixelDataEndpoint::new();
    let (mut gud, gud_handle) = Custom::builder()
        .with_interface(
            Interface::new(Class::vendor_specific(Class::VENDOR_SPECIFIC, 0), "GUD")
                .with_endpoint(gud_data_ep),
        )
        .build();

    let _reg = Gadget::new(
        Class::interface_specific(),
        gud_gadget::OPENMOKO_GUD_ID,
        Strings::new("The Internet", "Generic USB Display", ""),
    )
    .with_config(Config::new("gud").with_function(gud_handle))
    .bind(&udc)
    .expect("UDC binding failed");

    let running = Arc::new(AtomicBool::new(true));

    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("cleanup handler registration failed");

    let mode = connector.modes().first().unwrap();

    println!("picked mode {:?}", mode);

    let (width, height) = mode.size();
    let mut db = card
        // .create_dumb_buffer((width.into(), height.into()), drm::buffer::DrmFourcc::Xrgb8888, 32)
        .create_dumb_buffer(
            (width.into(), height.into()),
            drm::buffer::DrmFourcc::Rgb565,
            16,
        )
        .expect("Could not create dumb buffer");

    let fb = card
        .add_framebuffer(&db, 16, 16)
        .expect("Could not create FB");
    card.set_crtc(
        crtc.handle(),
        Some(fb),
        (0, 0),
        &[connector.handle()],
        Some(*mode),
    )
    .expect("Could not set CRTC");

    let pitch = db.pitch();

    let mut mapping = card
        .map_dumb_buffer(&mut db)
        .expect("map_dumb_buffer failed");

    while running.load(Ordering::Relaxed) {
        let event = gud
            .event_timeout(Duration::from_millis(100))
            .expect("read GUD event");
        if event.is_none() {
            continue;
        }
        let event = event.unwrap();

        if let Ok(Some(gud_event)) = gud_gadget::event(event) {
            match gud_event {
                Event::GetDescriptor(req) => {
                    req.send_descriptor(min_width, min_height, max_width, max_height)
                        .expect("failed to send descriptor");
                }
                Event::GetPixelFormats(req) => {
                    req.send_pixel_formats(&[gud_gadget::GUD_PIXEL_FORMAT_RGB565]).unwrap()
                }
                Event::GetDisplayModes(req) => {
                    let modes = card
                        .get_modes(connector.handle())
                        .unwrap()
                        .iter()
                        .map(|mode| {
                            let (hdisplay, vdisplay) = mode.size();
                            let (hsync_start, hsync_end, htotal) = mode.hsync();
                            let (vsync_start, vsync_end, vtotal) = mode.vsync();
                            DisplayMode {
                                clock: mode.clock(),
                                hdisplay,
                                htotal,
                                hsync_end,
                                hsync_start,
                                vtotal,
                                vdisplay,
                                vsync_end,
                                vsync_start,
                                flags: 0,
                            }
                        })
                        .collect::<Vec<DisplayMode>>();
                    req.send_modes(&modes).expect("failed to send modes");
                }
                Event::Buffer(info) => {
                    gud_data
                        .recv_buffer(info, mapping.as_mut(), pitch as usize, 2)
                        .expect("recv_buffer failed");
                }
            }
        }
    }

    Ok(())
}
