use anyhow::{bail, Context};
use dma_heap::{Heap, HeapKind};
use drm::buffer::Buffer;
use drm::control::Device;
use gud_gadget::{
    event as gud_event, pixel_data_endpoint, read_functionfs_event, Event, GudDisplayMode,
    GUD_DISPLAY_MODE_FLAG_PREFERRED,
};
use lz4::block;
use memmap2::{MmapMut, MmapOptions};
use nix::errno::Errno;
use nix::ioctl_readwrite;
use nix::ioctl_write_ptr;
use nix::libc::c_int;
use nix::poll::{poll, PollFd, PollFlags};
use std::env::args;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::path::Path;
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

    let (mut ep0, mut bulk_ep) = init_functionfs(&mut gud, &desc_data, &string_data)?;

    reg.bind(Some(&udc)).expect("UDC binding failed");

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("cleanup handler registration failed");

    let mode = connector
        .modes()
        .first()
        .expect("connector must have at least one mode");
    let preferred_mode = *mode;
    let connector_edid = read_connector_edid(&card_path, &connector, preferred_mode);
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
    let max_transfer_size = (max_width * max_height * 4) as usize;
    let mut dma_transfer = DmaTransfer::new(&dma_heap, max_transfer_size)?;
    dma_transfer
        .bind_endpoint(bulk_ep.as_raw_fd())
        .context("bind dma-buf to initial bulk endpoint")?;

    while running.load(Ordering::Relaxed) {
        if !wait_for_event(&ep0, Duration::from_millis(100))? {
            continue;
        }

        let event = loop {
            let result = read_functionfs_event(&mut ep0);
            if result.is_ok() {
                break result.unwrap();
            }
            let err = result.err().unwrap();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if is_ep0_disconnect(&err) {
                tracing::info!("FunctionFS endpoint closed ({err:?}); reopening endpoints");
                match reopen_functionfs(&mut gud) {
                    Ok((new_ep0, new_bulk)) => {
                        ep0 = new_ep0;
                        bulk_ep = new_bulk;
                        dma_transfer
                            .bind_endpoint(bulk_ep.as_raw_fd())
                            .context("bind dma-buf to reopened bulk endpoint")?;
                        tracing::info!("FunctionFS endpoints reopened");
                        continue;
                    }
                    Err(reopen_err) => {
                        return Err(reopen_err);
                    }
                }
            }
            return Err(err.into());
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
                    if req.connector() == 0 {
                        let mode = gud_mode_from_drm(preferred_mode, true);
                        req.send_modes(&[mode]).expect("failed to send modes");
                    } else {
                        req.send_modes(&[]).expect("failed to send modes");
                    }
                }
                Event::GetEdid(req) => {
                    if req.connector() == 0 {
                        if connector_edid.is_empty() {
                            req.send_edid(&[]).expect("failed to send EDID");
                        } else {
                            let len = connector_edid.len().min(req.max_len());
                            req.send_edid(&connector_edid[..len])
                                .expect("failed to send EDID");
                        }
                    } else {
                        req.send_edid(&[]).expect("failed to send EDID");
                    }
                }
                Event::Buffer(info) => {
                    handle_buffer(
                        &mut dma_transfer,
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

fn reopen_functionfs(gud: &mut Custom) -> anyhow::Result<(File, File)> {
    let ffs_dir = gud.ffs_dir().context("get functionfs dir")?;
    let ep0 = OpenOptions::new()
        .read(true)
        .write(true)
        .open(ffs_dir.join("ep0"))
        .context("reopen ep0")?;
    let bulk = OpenOptions::new()
        .read(true)
        .write(true)
        .open(ffs_dir.join("ep1"))
        .context("reopen bulk endpoint")?;
    Ok((ep0, bulk))
}

fn is_ep0_disconnect(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe
    )
}

fn handle_buffer(
    dma_transfer: &mut DmaTransfer,
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

    dma_transfer.receive(ep_fd, len)?;
    let cpu_guard = dma_transfer.cpu_read_guard()?;

    {
        let data: &[u8] = if info.compression > 0 {
            decompress_buf.resize(info.length as usize, 0);
            block::decompress_to_buffer(
                dma_transfer.bytes(),
                Some(info.length as i32),
                decompress_buf,
            )
            .context("lz4 decompress")?;
            &decompress_buf[..info.length as usize]
        } else {
            dma_transfer.bytes()
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
    }
    cpu_guard.finish()?;

    Ok(())
}

struct DmaTransfer {
    file: File,
    map: MmapMut,
    attached_ep: Option<RawFd>,
    valid_len: usize,
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
        Ok(Self {
            file,
            map,
            attached_ep: None,
            valid_len: 0,
        })
    }

    fn capacity(&self) -> usize {
        self.map.len()
    }

    fn bytes(&self) -> &[u8] {
        &self.map[..self.valid_len]
    }

    fn cpu_read_guard(&self) -> anyhow::Result<DmaBufCpuReadGuard<'_>> {
        DmaBufCpuReadGuard::new(self)
    }

    fn sync_for_cpu_start(&self) -> anyhow::Result<()> {
        sync_dmabuf(self.file.as_raw_fd(), DMA_BUF_SYNC_READ, false)
    }

    fn sync_for_cpu_end(&self) -> anyhow::Result<()> {
        sync_dmabuf(self.file.as_raw_fd(), DMA_BUF_SYNC_READ, true)
    }

    fn bind_endpoint(&mut self, ep_fd: RawFd) -> anyhow::Result<()> {
        if self.attached_ep == Some(ep_fd) {
            return Ok(());
        }
        if let Some(current_ep) = self.attached_ep.take() {
            let mut detach_fd = self.file.as_raw_fd() as c_int;
            tracing::trace!(
                "Detaching dma-buf fd {} from previous ep fd {}",
                detach_fd,
                current_ep
            );
            unsafe { ffs_dmabuf_detach(current_ep, &mut detach_fd) }
                .context("detach dma buffer")?;
        }
        let mut buffer_fd = self.file.as_raw_fd() as c_int;
        tracing::trace!(
            "Attaching dma-buf fd {} to ep fd {} (capacity {} bytes)",
            buffer_fd,
            ep_fd,
            self.capacity()
        );
        unsafe { ffs_dmabuf_attach(ep_fd, &mut buffer_fd) }.context("attach dma buffer")?;
        self.attached_ep = Some(ep_fd);
        Ok(())
    }

    fn receive(&mut self, ep_fd: RawFd, len: usize) -> anyhow::Result<()> {
        if len > self.capacity() {
            bail!(
                "transfer length {} exceeds dma-buf capacity {}",
                len,
                self.capacity()
            );
        }
        self.bind_endpoint(ep_fd)?;
        let mut req = UsbFfsDmabufTransferReq {
            fd: self.file.as_raw_fd(),
            flags: 0,
            length: len as u64,
        };
        tracing::trace!(
            "Submitting FUNCTIONFS_DMABUF_TRANSFER fd {} len {}",
            req.fd,
            req.length
        );
        let transfer_res = unsafe { ffs_dmabuf_transfer(ep_fd, &mut req) };
        transfer_res.context("transfer dma buffer")?;
        tracing::trace!("Waiting for dma-buf fence on fd {}", self.file.as_raw_fd());
        wait_for_dmabuf(self.file.as_raw_fd()).context("wait for dma buffer")?;
        tracing::trace!("Fence signalled for fd {}", self.file.as_raw_fd());
        self.valid_len = len;
        Ok(())
    }
}

impl Drop for DmaTransfer {
    fn drop(&mut self) {
        if let Some(ep_fd) = self.attached_ep {
            let mut detach_fd = self.file.as_raw_fd() as c_int;
            let _ = unsafe { ffs_dmabuf_detach(ep_fd, &mut detach_fd) };
            self.attached_ep = None;
        }
    }
}

struct DmaBufCpuReadGuard<'a> {
    transfer: &'a DmaTransfer,
    finished: bool,
}

impl<'a> DmaBufCpuReadGuard<'a> {
    fn new(transfer: &'a DmaTransfer) -> anyhow::Result<Self> {
        transfer.sync_for_cpu_start()?;
        Ok(Self {
            transfer,
            finished: false,
        })
    }

    fn finish(mut self) -> anyhow::Result<()> {
        self.finished = true;
        self.transfer.sync_for_cpu_end()
    }
}

impl Drop for DmaBufCpuReadGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.transfer.sync_for_cpu_end();
        }
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

fn gud_mode_from_drm(mode: drm::control::Mode, preferred: bool) -> GudDisplayMode {
    let (hdisplay, vdisplay) = mode.size();
    let (hsync_start, hsync_end, htotal) = mode.hsync();
    let (vsync_start, vsync_end, vtotal) = mode.vsync();
    let mut flags = 0;
    if preferred {
        flags |= GUD_DISPLAY_MODE_FLAG_PREFERRED;
    }

    GudDisplayMode {
        clock: mode.clock(),
        hdisplay,
        hsync_start,
        hsync_end,
        htotal,
        vdisplay,
        vsync_start,
        vsync_end,
        vtotal,
        flags,
    }
}

#[repr(C)]
struct DmaBufExportSyncFile {
    flags: u32,
    fd: i32,
}

const DMA_BUF_SYNC_READ: u32 = 1 << 0;
const DMA_BUF_SYNC_WRITE: u32 = 1 << 1;
const DMA_BUF_SYNC_RW: u32 = DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE;

ioctl_readwrite!(dma_buf_export_sync_file, b'b', 2, DmaBufExportSyncFile);

#[repr(C)]
struct DmaBufSync {
    flags: u64,
}

const DMA_BUF_SYNC_END: u64 = 1 << 2;

ioctl_write_ptr!(dma_buf_sync_ioctl, b'b', 0, DmaBufSync);

fn sync_dmabuf(dmabuf_fd: RawFd, flags: u32, end: bool) -> anyhow::Result<()> {
    let mut req = DmaBufSync {
        flags: u64::from(flags) | if end { DMA_BUF_SYNC_END } else { 0 },
    };
    unsafe {
        dma_buf_sync_ioctl(dmabuf_fd, &mut req)
            .map_err(|e| Errno::from_i32(e as i32))
            .context("sync dma-buf")?;
    }
    Ok(())
}

fn wait_for_dmabuf(dmabuf_fd: RawFd) -> anyhow::Result<()> {
    let mut req = DmaBufExportSyncFile {
        flags: DMA_BUF_SYNC_RW,
        fd: -1,
    };
    unsafe {
        dma_buf_export_sync_file(dmabuf_fd, &mut req)
            .map_err(|e| Errno::from_i32(e as i32))
            .context("export dma-buf sync file")?;
    }
    if req.fd < 0 {
        return Ok(());
    }
    let sync_fd = unsafe { OwnedFd::from_raw_fd(req.fd) };
    let mut fds = [PollFd::new(&sync_fd, PollFlags::POLLIN)];
    loop {
        match poll(&mut fds, -1) {
            Ok(_) => {
                if let Some(events) = fds[0].revents() {
                    if events.contains(PollFlags::POLLERR) {
                        return Err(anyhow::anyhow!("dma-buf transfer error"));
                    }
                    if events.contains(PollFlags::POLLIN) {
                        break;
                    }
                }
            }
            Err(Errno::EINTR) => continue,
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn read_connector_edid(
    card_path: &str,
    connector: &drm::control::connector::Info,
    preferred_mode: drm::control::Mode,
) -> Vec<u8> {
    let card_name = Path::new(card_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("card0");
    let connector_name = format!(
        "{}-{}-{}",
        card_name,
        connector.interface().as_str(),
        connector.interface_id()
    );
    let path = Path::new("/sys/class/drm")
        .join(connector_name)
        .join("edid");
    if let Ok(data) = fs::read(&path) {
        if !data.is_empty() {
            tracing::info!(
                "Loaded {} bytes of EDID data from {}",
                data.len(),
                path.display()
            );
            return data;
        }
        tracing::warn!("EDID file {} was empty", path.display());
    } else {
        tracing::warn!("Failed to read EDID from {}", path.display());
    }

    let generated = generate_edid(preferred_mode);
    tracing::info!(
        "Generated {}-byte synthetic EDID for connector {}",
        generated.len(),
        connector.interface().as_str()
    );
    generated
}

fn generate_edid(mode: drm::control::Mode) -> Vec<u8> {
    let mut edid = [0u8; 128];
    edid[0..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
    let vendor = encode_mfg_id(*b"GUD");
    edid[8..10].copy_from_slice(&vendor.to_be_bytes());
    edid[10..12].copy_from_slice(&0x0001u16.to_le_bytes());
    edid[12..16].copy_from_slice(&0u32.to_le_bytes());
    edid[16] = 1; // manufacture week
    edid[17] = 34; // 1990 + 34 = 2024
    edid[18] = 1;
    edid[19] = 4;
    edid[20] = 0x80; // digital input
    edid[21] = 0;
    edid[22] = 0;
    edid[23] = 0x78; // gamma 2.2
    edid[24] = 0x0a; // sRGB + preferred timing
                     // chromaticity defaults (approx sRGB)
    edid[25..35].copy_from_slice(&[0x6f, 0x8a, 0x6a, 0x57, 0x4a, 0x9f, 0x26, 0x10, 0x45, 0x46]);
    // established timings/standard timings zeroed
    edid[35..38].fill(0);
    edid[38..54].fill(0x01);

    let detailed = detailed_timing_descriptor(mode);
    edid[54..72].copy_from_slice(&detailed);
    edid[72..90].copy_from_slice(&descriptor_string(0xFC, "GUD Display"));
    edid[90..108].copy_from_slice(&descriptor_string(0xFE, "GUD Panel"));
    edid[108..126].fill(0);

    edid[126] = 0; // no extensions
    let sum: u16 = edid.iter().take(127).map(|b| *b as u16).sum();
    edid[127] = ((256 - (sum % 256)) & 0xFF) as u8;
    edid.to_vec()
}

fn encode_mfg_id(tag: [u8; 3]) -> u16 {
    let mut value = 0u16;
    for (i, ch) in tag.iter().enumerate() {
        let letter = (ch.to_ascii_uppercase() - b'@') as u16;
        value |= letter << (10 - (i as u16 * 5));
    }
    value
}

fn descriptor_string(tag: u8, text: &str) -> [u8; 18] {
    let mut desc = [0u8; 18];
    desc[3] = tag;
    let mut buf = [b' '; 13];
    let bytes = text.as_bytes();
    let len = bytes.len().min(12);
    buf[..len].copy_from_slice(&bytes[..len]);
    buf[len] = 0x0a;
    desc[5..18].copy_from_slice(&buf);
    desc
}

fn detailed_timing_descriptor(mode: drm::control::Mode) -> [u8; 18] {
    let mut dtd = [0u8; 18];
    let (hdisplay, vdisplay) = mode.size();
    let (hsync_start, hsync_end, htotal) = mode.hsync();
    let hblank = htotal - hdisplay;
    let hsync_offset = hsync_start - hdisplay;
    let hsync_width = hsync_end - hsync_start;

    let (vsync_start, vsync_end, vtotal) = mode.vsync();
    let vblank = vtotal - vdisplay;
    let vsync_offset = vsync_start - vdisplay;
    let vsync_width = vsync_end - vsync_start;

    let pixel_clock = (mode.clock() / 10) as u16;
    dtd[0..2].copy_from_slice(&pixel_clock.to_le_bytes());
    dtd[2] = (hdisplay & 0xff) as u8;
    dtd[3] = (hblank & 0xff) as u8;
    dtd[4] = (((hdisplay >> 8) & 0xF) << 4 | ((hblank >> 8) & 0xF)) as u8;
    dtd[5] = (hsync_offset & 0xff) as u8;
    dtd[6] = (hsync_width & 0xff) as u8;
    dtd[7] = ((((hsync_offset >> 8) & 0x3) << 6)
        | (((hsync_width >> 8) & 0x3) << 4)
        | (((vsync_offset >> 4) & 0x3) << 2)
        | ((vsync_width >> 4) & 0x3)) as u8;
    dtd[8] = (vdisplay & 0xff) as u8;
    dtd[9] = (vblank & 0xff) as u8;
    dtd[10] = (((vdisplay >> 8) & 0xF) << 4 | ((vblank >> 8) & 0xF)) as u8;
    dtd[11] = (((vsync_offset & 0xF) << 4) | (vsync_width & 0xF)) as u8;
    dtd[12] = 0; // image size unknown
    dtd[13] = 0;
    dtd[14] = 0;
    dtd[15] = 0;
    dtd[16] = 0;
    dtd[17] = 0x00;
    dtd
}
