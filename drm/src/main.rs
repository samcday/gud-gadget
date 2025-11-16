use anyhow::Context;
use dma_heap::{Heap, HeapKind};
use drm::buffer::Buffer;
use drm::control::Device;
use gud_gadget::{
    event as gud_event, pixel_data_endpoint, read_functionfs_event, DisplayMode, Event,
};
use lz4::block;
use memmap2::{MmapMut, MmapOptions};
use nix::errno::Errno;
use nix::ioctl_write_ptr;
use nix::libc::c_int;
use nix::poll::{poll, PollFd, PollFlags};
use std::env::args;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use usb_gadget::function::custom::{Custom, Interface};
use usb_gadget::{default_udc, Class, Config, Gadget, Strings};

#[derive(Debug)]
pub struct Card(std::fs::File);

impl std::os::unix::io::AsFd for Card {
    fn as_fd(&self) -> std::os::unix::io::BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for Card {}
impl Device for Card {}

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
        .nth(1)
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
        min_width = min_width.min(width);
        max_width = max_width.max(width);
        min_height = min_height.min(height);
        max_height = max_height.max(height);
    }

    usb_gadget::remove_all().expect("UDC init failed");

    let mut builder = Custom::builder().with_interface(
        Interface::new(Class::vendor_specific(Class::VENDOR_SPECIFIC, 0), "GUD")
            .with_endpoint(pixel_data_endpoint()),
    );
    builder.ffs_no_init = true;
    let (desc_data, string_data) = builder
        .ffs_descriptors_and_strings()
        .context("build functionfs descriptors")?;
    let (mut gud, gud_handle) = builder.build();

    let reg = Gadget::new(
        Class::interface_specific(),
        gud_gadget::OPENMOKO_GUD_ID,
        Strings::new("The Internet", "Generic USB Display", ""),
    )
    .with_config(Config::new("gud").with_function(gud_handle))
    .register()
    .expect("gadget registration failed");

    let (mut ep0, bulk_ep) = init_functionfs(&mut gud, &desc_data, &string_data)?;

    reg.bind(Some(&udc)).expect("UDC binding failed");

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

    let dma_heap = Heap::new(HeapKind::System).context("open dma heap")?;
    let mut decompress_buf = Vec::new();

    while running.load(Ordering::Relaxed) {
        if !wait_for_event(&ep0, Duration::from_millis(100))? {
            continue;
        }

        let event = match read_functionfs_event(&mut ep0) {
            Ok(event) => event,
            Err(err) if is_ep0_disconnect(&err) => {
                tracing::info!("FunctionFS endpoint closed ({err:?}); stopping");
                break;
            }
            Err(err) => return Err(err.into()),
        };

        if let Ok(Some(gud_event)) = gud_event(event) {
            match gud_event {
                Event::GetDescriptor(req) => {
                    req.send_descriptor(min_width, min_height, max_width, max_height)
                        .expect("failed to send descriptor");
                }
                Event::GetPixelFormats(req) => {
                    req.send_pixel_formats(&[gud_gadget::GUD_PIXEL_FORMAT_RGB565])
                        .expect("failed to send pixel formats");
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
                    handle_buffer(
                        &dma_heap,
                        bulk_ep.as_raw_fd(),
                        info,
                        mapping.as_mut(),
                        pitch as usize,
                        2,
                        &mut decompress_buf,
                    )
                    .expect("recv_buffer failed");
                }
            }
        }
    }

    Ok(())
}

fn wait_for_event(ep0: &File, timeout: Duration) -> anyhow::Result<bool> {
    let mut fds = [PollFd::new(ep0, PollFlags::POLLIN)];
    let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    loop {
        match poll(&mut fds, timeout_ms) {
            Ok(0) => return Ok(false),
            Ok(_) => {
                return Ok(fds[0]
                    .revents()
                    .map(|events| events.contains(PollFlags::POLLIN))
                    .unwrap_or(false))
            }
            Err(err) if err == Errno::EINTR => continue,
            Err(err) => return Err(err.into()),
        }
    }
}

fn init_functionfs(
    gud: &mut Custom,
    desc_data: &[u8],
    string_data: &[u8],
) -> anyhow::Result<(File, File)> {
    let ffs_dir = gud.ffs_dir().context("get functionfs dir")?;
    let mut ep0 = OpenOptions::new()
        .read(true)
        .write(true)
        .open(ffs_dir.join("ep0"))
        .context("open ep0")?;
    ep0.write_all(desc_data).context("write descriptors")?;
    ep0.write_all(string_data).context("write strings")?;
    let bulk = OpenOptions::new()
        .read(true)
        .write(true)
        .open(ffs_dir.join("ep1"))
        .context("open bulk endpoint")?;
    Ok((ep0, bulk))
}

fn is_ep0_disconnect(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe
    )
}

fn handle_buffer(
    heap: &Heap,
    ep_fd: RawFd,
    info: gud_gadget::SetBuffer,
    fb: &mut [u8],
    fb_pitch: usize,
    bpp: usize,
    decompress_buf: &mut Vec<u8>,
) -> anyhow::Result<()> {
    let len = if info.compression > 0 {
        info.compressed_length
    } else {
        info.length
    } as usize;

    let mut transfer = DmaTransfer::new(heap, len)?;
    transfer.receive(ep_fd, len)?;

    let data: &[u8] = if info.compression > 0 {
        decompress_buf.resize(info.length as usize, 0);
        block::decompress_to_buffer(transfer.bytes(), Some(info.length as i32), decompress_buf)
            .context("lz4 decompress")?;
        &decompress_buf[..info.length as usize]
    } else {
        transfer.bytes()
    };

    let mut y = info.y as usize;
    let end_y = (info.y + info.height) as usize;
    let line_len = info.width as usize * bpp;
    let line_start = info.x as usize * bpp;
    let mut buf_pos = 0usize;
    while y < end_y {
        let fb_start = (y * fb_pitch) + line_start;
        let fb_end = fb_start + line_len;
        fb[fb_start..fb_end].copy_from_slice(&data[buf_pos..buf_pos + line_len]);
        buf_pos += line_len;
        y += 1;
    }

    Ok(())
}

struct DmaTransfer {
    file: File,
    map: MmapMut,
}

impl DmaTransfer {
    fn new(heap: &Heap, len: usize) -> anyhow::Result<Self> {
        let fd = heap.allocate(len).context("allocate dma buffer")?;
        let file = unsafe { File::from_raw_fd(fd.into_raw_fd()) };
        let map = unsafe {
            MmapOptions::new()
                .len(len)
                .map_mut(&file)
                .context("mmap dma buffer")?
        };
        Ok(Self { file, map })
    }

    fn bytes(&self) -> &[u8] {
        &self.map
    }

    fn receive(&mut self, ep_fd: RawFd, len: usize) -> anyhow::Result<()> {
        let mut buffer_fd = self.file.as_raw_fd() as c_int;
        unsafe { ffs_dmabuf_attach(ep_fd, &mut buffer_fd) }.context("attach dma buffer")?;
        let mut req = UsbFfsDmabufTransferReq {
            fd: self.file.as_raw_fd(),
            flags: 0,
            length: len as u64,
        };
        let transfer_res = unsafe { ffs_dmabuf_transfer(ep_fd, &mut req) };
        let mut detach_fd = self.file.as_raw_fd() as c_int;
        let detach_res = unsafe { ffs_dmabuf_detach(ep_fd, &mut detach_fd) };
        transfer_res.context("transfer dma buffer")?;
        detach_res.context("detach dma buffer")?;
        Ok(())
    }
}

#[repr(C)]
struct UsbFfsDmabufTransferReq {
    fd: i32,
    flags: u32,
    length: u64,
}

ioctl_write_ptr!(ffs_dmabuf_attach, b'g', 131, c_int);
ioctl_write_ptr!(ffs_dmabuf_detach, b'g', 132, c_int);
ioctl_write_ptr!(ffs_dmabuf_transfer, b'g', 133, UsbFfsDmabufTransferReq);
