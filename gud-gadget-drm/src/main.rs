use std::env::args;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use drm::buffer::Buffer;
use drm::control::Device;
use gud_gadget::{DisplayMode, Event};
use usb_gadget::{Class, Config, default_udc, Gadget, Id, Strings};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

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

    let card_path = args().skip(1).next().expect("specify full path to /dev/dri/cardN as program argument");
    let card = Card::open(&card_path);
    let udc = default_udc().expect("no UDC found");

    let resources = card.resource_handles().expect("load drm resources failed");

    let connector = resources.connectors().iter()
        .map(|h| card.get_connector(*h, false).expect("get drm connector failed"))
        .find(|c| c.state() == drm::control::connector::State::Connected)
        .expect("no connected connectors found");

    let crtc = resources.crtcs()
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
        if width < min_width { min_width = width }
        if width > max_width { max_width = width }
        if height < min_height { min_height = height }
        if height > max_height { max_height = height }
    }

    usb_gadget::remove_all().expect("UDC init failed");

    let (mut gud, mut gud_data, handle) = gud_gadget::Function::new();

    let _reg = Gadget::new(Class::new(255, 255, 3), Id::new(0x1d50, 0x614d), Strings::new("foo", "GUD", "666"))
        .with_config(Config::new("gud").with_function(handle))
        .bind(&udc)
        .expect("UDC binding failed");

    let running = Arc::new(AtomicBool::new(true));

    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    }).expect("cleanup handler registration failed");

    let mode = connector.modes().first().unwrap();

    println!("picked mode {:?}", mode);

    let (width, height) = mode.size();
    let mut db = card
        // .create_dumb_buffer((width.into(), height.into()), drm::buffer::DrmFourcc::Xrgb8888, 32)
        .create_dumb_buffer((width.into(), height.into()), drm::buffer::DrmFourcc::Rgb565, 16)
        .expect("Could not create dumb buffer");

    let fb = card
        .add_framebuffer(&db, 16, 16)
        .expect("Could not create FB");
    card.set_crtc(crtc.handle(), Some(fb), (0, 0), &[connector.handle()], Some(*mode))
        .expect("Could not set CRTC");

    let pitch = db.pitch();

    let mut mapping = card.map_dumb_buffer(&mut db).expect("map_dumb_buffer failed");

    while running.load(Ordering::Relaxed) {
        if let Ok(Some(event)) = gud.event(Duration::from_millis(100)) {
            match event {
                Event::GetDescriptorRequest(req) => {
                    req.send_descriptor(min_width, min_height, max_width, max_height).expect("failed to send descriptor");
                },
                Event::GetDisplayModesRequest(req) => {
                    let modes = card.get_modes(connector.handle()).unwrap().iter()
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
                    // req.send_modes(&modes).expect("failed to send modes");
                    req.send_modes(&[DisplayMode{
                        clock: 60 * (width as u32) * (height as u32) / 1000,
                            hdisplay: width,
                            htotal: width,
                            hsync_end: width,
                            hsync_start: width,
                            vtotal: height,
                            vdisplay: height,
                            vsync_end: height,
                            vsync_start: height,
                            flags: 0,
                    }]).expect("failed to send modes");
                },
                Event::Buffer(info) => {
                    gud_data.recv_buffer(info, mapping.as_mut(), pitch as usize).expect("recv_buffer failed");
                }
            }
        }
    }

    Ok(())
}
