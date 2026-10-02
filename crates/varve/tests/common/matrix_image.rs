//! Test-only logical matrix image access. Existing corruption fixtures address
//! logical layout offsets; translate those to the confirmed COW page, and
//! reseal the outer page/directory CRC so the *inner* integrity invariant under
//! test is exercised. Generation-store CRC corruption is tested separately.
#![allow(dead_code)]
use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
const PAGE: usize = 4096;
fn crc(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ ((0u32.wrapping_sub(crc & 1)) & 0xedb88320);
        }
    }
    !crc
}
fn num(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}
fn read_at(f: &mut File, at: u64, buf: &mut [u8]) -> io::Result<()> {
    f.seek(SeekFrom::Start(at))?;
    f.read_exact(buf)
}
fn write_at(f: &mut File, at: u64, buf: &[u8]) -> io::Result<()> {
    f.seek(SeekFrom::Start(at))?;
    f.write_all(buf)
}
fn seal(bytes: &mut [u8; PAGE]) {
    let c = crc(&bytes[..PAGE - 4]);
    bytes[PAGE - 4..].copy_from_slice(&c.to_le_bytes());
}
pub struct OpenOptions {
    inner: std::fs::OpenOptions,
    writable: bool,
}
impl OpenOptions {
    pub fn new() -> Self {
        Self {
            inner: std::fs::OpenOptions::new(),
            writable: false,
        }
    }
    pub fn read(&mut self, b: bool) -> &mut Self {
        self.inner.read(b);
        self
    }
    pub fn write(&mut self, b: bool) -> &mut Self {
        self.inner.write(b);
        self.writable = b;
        self
    }
    pub fn create(&mut self, b: bool) -> &mut Self {
        self.inner.create(b);
        self
    }
    pub fn create_new(&mut self, b: bool) -> &mut Self {
        self.inner.create_new(b);
        self
    }
    pub fn truncate(&mut self, b: bool) -> &mut Self {
        self.inner.truncate(b);
        self
    }
    pub fn append(&mut self, b: bool) -> &mut Self {
        self.inner.append(b);
        self
    }
    pub fn open(&self, path: impl AsRef<Path>) -> io::Result<Image> {
        let path = path.as_ref();
        let base = self.inner.open(path)?;
        let mut shadow = None;
        if let Ok(mut probe) = File::open(path) {
            let mut prefix = vec![0; probe.metadata()?.len().min(65536) as usize];
            probe.read_exact(&mut prefix)?;
            if let Some(at) = prefix.windows(4).position(|b| b == b"VMNC")
                && let Some(nonce) = prefix.get(at + 8..at + 24)
            {
                let mut name = path.as_os_str().to_os_string();
                name.push(".");
                for byte in nonce {
                    name.push(format!("{byte:02x}"));
                }
                name.push(".vmg");
                let name = PathBuf::from(name);
                if name.exists() {
                    let log = std::fs::OpenOptions::new()
                        .read(true)
                        .write(self.writable)
                        .open(name)?;
                    shadow = Some((log, (at + 24) as u64));
                }
            }
        }
        Ok(Image {
            base,
            shadow,
            cursor: 0,
        })
    }
}
pub struct Image {
    base: File,
    shadow: Option<(File, u64)>,
    cursor: u64,
}
impl Image {
    pub fn metadata(&self) -> io::Result<Metadata> {
        self.base.metadata()
    }
    pub fn set_len(&self, len: u64) -> io::Result<()> {
        self.base.set_len(len)
    }
    pub fn sync_all(&self) -> io::Result<()> {
        self.base.sync_all()?;
        if let Some((log, _)) = &self.shadow {
            log.sync_all()?;
        }
        Ok(())
    }
    pub fn sync_data(&self) -> io::Result<()> {
        self.sync_all()
    }
    fn location(&mut self, page: u64) -> io::Result<Option<(u64, u64, usize, u64)>> {
        let Some((log, _)) = &mut self.shadow else {
            return Ok(None);
        };
        let mut header = [0; PAGE];
        read_at(log, 0, &mut header)?;
        let len = num(&header, 24);
        if page * PAGE as u64 >= len {
            return Ok(None);
        }
        let mut head = None;
        for at in [4096, 8192] {
            let mut bytes = [0; PAGE];
            read_at(log, at, &mut bytes)?;
            if &bytes[..8] == b"VMGHEAD1"
                && crc(&bytes[..PAGE - 4])
                    == u32::from_le_bytes(bytes[PAGE - 4..].try_into().unwrap())
                && head
                    .as_ref()
                    .is_none_or(|(_, h): &(u64, [u8; PAGE])| num(h, 8) < num(&bytes, 8))
            {
                head = Some((at, bytes));
            }
        }
        let Some((head_at, head)) = head else {
            return Ok(None);
        };
        let mut root = num(&head, 16);
        let mut height = 1;
        let pages = (len - 1) / PAGE as u64;
        while pages >> (height * 7) != 0 {
            height += 1;
        }
        for h in (0..height).rev() {
            if root == 0 {
                return Ok(None);
            }
            let mut node = [0; PAGE];
            read_at(log, root, &mut node)?;
            let at = 32 + (((page >> (h * 7)) & 127) as usize) * 16;
            let data = num(&node, at);
            if h == 0 {
                return Ok(Some((data, root, at, head_at)));
            }
            root = data;
        }
        Ok(None)
    }
}
impl Seek for Image {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let at = match from {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => self.cursor as i128 + n as i128,
            SeekFrom::End(n) => self.base.metadata()?.len() as i128 + n as i128,
        };
        self.cursor = u64::try_from(at).map_err(io::Error::other)?;
        Ok(self.cursor)
    }
}
impl Read for Image {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let n = bytes
            .len()
            .min(self.base.metadata()?.len().saturating_sub(self.cursor) as usize);
        let mut done = 0;
        while done < n {
            let at = self.cursor;
            let within = at as usize % PAGE;
            let take = (n - done).min(PAGE - within);
            let take = self
                .shadow
                .as_ref()
                .filter(|(_, start)| at < *start)
                .map_or(take, |(_, start)| take.min((*start - at) as usize));
            let use_shadow = self.shadow.as_ref().is_some_and(|(_, start)| at >= *start);
            match if use_shadow {
                self.location(at / PAGE as u64)?
            } else {
                None
            } {
                Some((1, _, _, _)) => bytes[done..done + take].fill(0),
                Some((offset, _, _, _)) if offset > 1 => read_at(
                    &mut self.shadow.as_mut().unwrap().0,
                    offset + within as u64,
                    &mut bytes[done..done + take],
                )?,
                _ => read_at(&mut self.base, at, &mut bytes[done..done + take])?,
            }
            self.cursor += take as u64;
            done += take;
        }
        Ok(n)
    }
}
impl Write for Image {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut done = 0;
        while done < bytes.len() {
            let at = self.cursor;
            let within = at as usize % PAGE;
            let take = (bytes.len() - done).min(PAGE - within);
            let take = self
                .shadow
                .as_ref()
                .filter(|(_, start)| at < *start)
                .map_or(take, |(_, start)| take.min((*start - at) as usize));
            let use_shadow = self.shadow.as_ref().is_some_and(|(_, start)| at >= *start);
            match if use_shadow {
                self.location(at / PAGE as u64)?
            } else {
                None
            } {
                Some((mut offset, node_at, entry_at, head_at)) if offset != 0 => {
                    let log = &mut self.shadow.as_mut().unwrap().0;
                    let mut page = [0; PAGE];
                    if offset > 1 {
                        read_at(log, offset, &mut page)?;
                    } else {
                        offset = log.metadata()?.len().div_ceil(PAGE as u64) * PAGE as u64;
                    }
                    page[within..within + take].copy_from_slice(&bytes[done..done + take]);
                    write_at(log, offset, &page)?;
                    let mut node = [0; PAGE];
                    read_at(log, node_at, &mut node)?;
                    node[entry_at..entry_at + 8].copy_from_slice(&offset.to_le_bytes());
                    node[entry_at + 8..entry_at + 12].copy_from_slice(&crc(&page).to_le_bytes());
                    seal(&mut node);
                    write_at(log, node_at, &node)?;
                    let mut head = [0; PAGE];
                    read_at(log, head_at, &mut head)?;
                    head[24..32].copy_from_slice(&log.metadata()?.len().to_le_bytes());
                    seal(&mut head);
                    write_at(log, head_at, &head)?;
                }
                _ => write_at(&mut self.base, at, &bytes[done..done + take])?,
            }
            self.cursor += take as u64;
            done += take;
        }
        Ok(done)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.base.flush()
    }
}
