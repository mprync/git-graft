//! Compare private CoW copies with the exact bytes Git would check out.
use super::{Checkout, Entry};
use anyhow::{Context, Result, ensure};
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, Write},
    process::{Child, ChildStdout, Stdio},
};

pub(super) fn digest(mut file: File) -> Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher.finalize())
}

struct Batch {
    child: Child,
    out: BufReader<ChildStdout>,
}

impl Batch {
    fn start(checkout: &Checkout<'_>, files: &[&Entry], filtered: bool) -> Result<Self> {
        // Feeding a real file avoids blocking on stdout while writing requests.
        let mut input = tempfile::tempfile()?;
        for entry in files {
            input.write_all(entry.blob.as_bytes())?;
            if filtered {
                input.write_all(b" ")?;
                input.write_all(&entry.name)?;
            }
            input.write_all(b"\0")?;
        }
        input.rewind()?;
        let mut command = checkout.command();
        command.args(["cat-file", "--batch", "-Z"]);
        if filtered {
            command.arg("--filters");
        }
        let mut child = command
            .stdin(Stdio::from(input))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let out =
            BufReader::with_capacity(64 * 1024, child.stdout.take().context("Git batch pipe")?);
        Ok(Self { child, out })
    }

    fn header(&mut self, entry: &Entry) -> Result<u64> {
        let mut header = Vec::new();
        self.out.read_until(0, &mut header)?;
        ensure!(header.pop() == Some(0), "incomplete Git object header");
        let header = std::str::from_utf8(&header)?;
        let size = header
            .strip_prefix(&entry.blob)
            .and_then(|rest| rest.strip_prefix(" blob "))
            .context("unexpected Git object header")?;
        Ok(size.parse()?)
    }

    fn end_blob(&mut self) -> Result<()> {
        let mut byte = [0];
        self.out.read_exact(&mut byte)?;
        ensure!(byte == [0], "unexpected Git object length");
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        ensure!(self.out.fill_buf()?.is_empty(), "extra Git batch data");
        ensure!(self.child.wait()?.success(), "Git object batch failed");
        Ok(())
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(super) struct RawBatch {
    batch: Batch,
    expected: Vec<u8>,
    actual: Vec<u8>,
}

impl RawBatch {
    pub(super) fn start(checkout: &Checkout<'_>, files: &[&Entry]) -> Result<Self> {
        Ok(Self {
            batch: Batch::start(checkout, files, false)?,
            expected: vec![0; 64 * 1024],
            actual: vec![0; 64 * 1024],
        })
    }

    pub(super) fn matches(
        &mut self,
        entry: &Entry,
        file: Option<(&mut File, u64)>,
    ) -> Result<bool> {
        let size = self.batch.header(entry)?;
        let mut input = file
            .filter(|(_, bytes)| *bytes == size)
            .map(|(file, _)| file);
        let mut remaining = size;
        while remaining != 0 {
            let length = remaining.min(self.expected.len() as u64) as usize;
            self.batch.out.read_exact(&mut self.expected[..length])?;
            if let Some(file) = &mut input {
                file.read_exact(&mut self.actual[..length])?;
                if self.actual[..length] != self.expected[..length] {
                    input = None;
                }
            }
            remaining -= length as u64;
        }
        self.batch.end_blob()?;
        Ok(input.is_some())
    }

    pub(super) fn finish(self) -> Result<()> {
        self.batch.finish()
    }
}

// Git reports the original blob length in --batch --filters even after
// conversion. A NUL could be file data. Frame the filtered stream with the raw
// stream, permitting only the CRLF insertion established by the attr check.
pub(super) fn checkout_digests(
    checkout: &Checkout<'_>,
    files: &[&Entry],
) -> Result<Vec<blake3::Hash>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let mut raw = Batch::start(checkout, files, false)?;
    if !checkout.may_convert_eol {
        let mut digests = Vec::with_capacity(files.len());
        for entry in files {
            let size = raw.header(entry)?;
            let mut hasher = blake3::Hasher::new();
            ensure!(
                std::io::copy(&mut (&mut raw.out).take(size), &mut hasher)? == size,
                "incomplete Git object"
            );
            raw.end_blob()?;
            digests.push(hasher.finalize());
        }
        raw.finish()?;
        return Ok(digests);
    }
    let mut filtered = Batch::start(checkout, files, true)?;
    let mut input = vec![0; 64 * 1024];
    let mut expected = Vec::with_capacity(input.len() * 2);
    let mut actual = vec![0; input.len() * 2];
    let mut digests = Vec::with_capacity(files.len());
    for entry in files {
        let mut remaining = raw.header(entry)?;
        ensure!(
            remaining == filtered.header(entry)?,
            "Git object header size changed"
        );
        let mut hasher = blake3::Hasher::new();
        let mut insert_cr = None;
        let mut prev_cr = false;
        while remaining > 0 {
            let len = remaining.min(input.len() as u64) as usize;
            remaining -= len as u64;
            let input = &mut input[..len];
            raw.out.read_exact(input)?;
            let cr_before_chunk = prev_cr;
            // Determine conversion at the first naked LF. Until then the
            // original and filtered streams must be byte-for-byte identical.
            if insert_cr.is_none()
                && let Some(lf) = input.iter().enumerate().position(|(i, &b)| {
                    b == b'\n'
                        && if i == 0 {
                            !prev_cr
                        } else {
                            input[i - 1] != b'\r'
                        }
                })
            {
                filtered.out.read_exact(&mut actual[..lf])?;
                ensure!(
                    actual[..lf] == input[..lf],
                    "unexpected checkout conversion"
                );
                hasher.update(&actual[..lf]);
                insert_cr = match filtered.out.fill_buf()?.first() {
                    Some(b'\r') => Some(true),
                    Some(b'\n') => Some(false),
                    _ => anyhow::bail!("unexpected checkout conversion"),
                };
                let input = &input[lf..];
                compare(
                    &mut filtered.out,
                    &mut expected,
                    &mut actual,
                    input,
                    insert_cr == Some(true),
                    false,
                    &mut hasher,
                )?;
            } else {
                compare(
                    &mut filtered.out,
                    &mut expected,
                    &mut actual,
                    input,
                    insert_cr == Some(true),
                    cr_before_chunk,
                    &mut hasher,
                )?;
            }
            prev_cr = input.last() == Some(&b'\r');
        }
        raw.end_blob()?;
        filtered.end_blob()?;
        digests.push(hasher.finalize());
    }
    raw.finish()?;
    filtered.finish()?;
    Ok(digests)
}

fn compare(
    filtered: &mut impl Read,
    converted: &mut Vec<u8>,
    actual: &mut [u8],
    input: &[u8],
    insert_cr: bool,
    mut prev_cr: bool,
    hasher: &mut blake3::Hasher,
) -> Result<()> {
    let expected = if insert_cr {
        converted.clear();
        for &byte in input {
            if byte == b'\n' && !prev_cr {
                converted.push(b'\r');
            }
            converted.push(byte);
            prev_cr = byte == b'\r';
        }
        converted.as_slice()
    } else {
        input
    };
    let actual = &mut actual[..expected.len()];
    filtered.read_exact(actual)?;
    ensure!(actual == expected, "unexpected checkout conversion");
    hasher.update(actual);
    Ok(())
}
