use drm::buffer::Buffer;
use drm::control::Device;
use gadgetry_most_foul::function::custom::{Custom, Interface};
use gadgetry_most_foul::{default_udc, Class, Config, Gadget, Strings};
use gud_gadget::{DisplayMode, Event};
use std::env::args;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

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

    let mut args = args().skip(1);
    let card_path = args.next().expect(
        "specify full path to /dev/dri/cardN as program argument, or --headless [WIDTHxHEIGHT]",
    );

    if card_path == "--headless" {
        let (width, height) = args
            .next()
            .map(|s| parse_size(&s))
            .transpose()?
            .unwrap_or((800, 600));
        return run_headless(width, height);
    }

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

    gadgetry_most_foul::remove_all().expect("UDC init failed");

    let (mut gud_data, gud_data_ep) = gud_gadget::PixelDataEndpoint::new();
    let (mut gud, gud_handle) = Custom::builder()
        .with_interface(
            Interface::new(Class::vendor_specific(Class::VENDOR_SPECIFIC, 0), "GUD")
                .with_endpoint(gud_data_ep),
        )
        .build();

    let _reg = Gadget::new(
        Class::INTERFACE_SPECIFIC,
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
        let event = match gud.event_timeout(Duration::from_millis(100)) {
            Ok(event) => event,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => break,
            Err(_) if !running.load(Ordering::Relaxed) => break,
            Err(err) => return Err(err.into()),
        };
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
                Event::GetPixelFormats(req) => req
                    .send_pixel_formats(&[gud_gadget::GUD_PIXEL_FORMAT_RGB565])
                    .unwrap(),
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

fn parse_size(size: &str) -> anyhow::Result<(u16, u16)> {
    let (width, height) = size
        .split_once('x')
        .ok_or_else(|| anyhow::anyhow!("expected size as WIDTHxHEIGHT"))?;
    Ok((width.parse()?, height.parse()?))
}

fn fixed_mode(width: u16, height: u16) -> DisplayMode {
    let hsync_start = width + 40;
    let hsync_end = hsync_start + 48;
    let htotal = hsync_end + 40;
    let vsync_start = height + 13;
    let vsync_end = vsync_start + 3;
    let vtotal = vsync_end + 29;

    DisplayMode {
        clock: htotal as u32 * vtotal as u32 * 60 / 1000,
        hdisplay: width,
        hsync_start,
        hsync_end,
        htotal,
        vdisplay: height,
        vsync_start,
        vsync_end,
        vtotal,
        flags: 0,
    }
}

fn run_headless(width: u16, height: u16) -> anyhow::Result<()> {
    let udc = default_udc().expect("no UDC found");

    gadgetry_most_foul::remove_all().expect("UDC init failed");

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

    let mode = fixed_mode(width, height);
    let pitch = width as usize * 2;
    let mut fb = vec![0; pitch * height as usize];

    println!("headless GUD gadget using mode {:?}", mode);

    while running.load(Ordering::Relaxed) {
        let event = match gud.event_timeout(Duration::from_millis(100)) {
            Ok(event) => event,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => break,
            Err(_) if !running.load(Ordering::Relaxed) => break,
            Err(err) => return Err(err.into()),
        };
        if event.is_none() {
            continue;
        }
        let event = event.unwrap();

        if let Ok(Some(gud_event)) = gud_gadget::event(event) {
            match gud_event {
                Event::GetDescriptor(req) => req
                    .send_descriptor(width.into(), height.into(), width.into(), height.into())
                    .expect("failed to send descriptor"),
                Event::GetPixelFormats(req) => req
                    .send_pixel_formats(&[gud_gadget::GUD_PIXEL_FORMAT_RGB565])
                    .unwrap(),
                Event::GetDisplayModes(req) => {
                    req.send_modes(&[fixed_mode(width, height)])
                        .expect("failed to send modes");
                }
                Event::Buffer(info) => gud_data
                    .recv_buffer(info, &mut fb, pitch, 2)
                    .expect("recv_buffer failed"),
            }
        }
    }

    Ok(())
}
