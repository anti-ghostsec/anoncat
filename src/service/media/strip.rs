//! Removes identifying metadata from uploaded images: EXIF (GPS position,
//! camera make and serial, timestamps), XMP, IPTC and comments.
//!
//! Only the metadata blocks are dropped; the picture's own data is copied as
//! it is, so nothing is re-encoded and quality is unchanged. The colour
//! profile and orientation-free pixel data are kept. Anything that doesn't
//! parse cleanly as JPEG, PNG or WebP (an encrypted upload, a truncated
//! file, another format) comes back untouched.

/// The file without its metadata, or `None` when there was none to remove
/// or the file isn't a well-formed JPEG, PNG or WebP.
#[must_use]
pub fn strip_metadata(file: &[u8]) -> Option<Vec<u8>> {
	match file {
		| [0xFF, 0xD8, ..] => jpeg(file),
		| [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, ..] => png(file),
		| [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => webp(file),
		| _ => None,
	}
}

/// JPEG: drops APP1 (EXIF, XMP), APP12, APP13 (IPTC/Photoshop) and COM
/// segments before the image data. APP0 (JFIF), APP2 (ICC profile) and
/// APP14 (Adobe colour transform) stay: they change how it looks.
fn jpeg(file: &[u8]) -> Option<Vec<u8>> {
	let mut out = Vec::with_capacity(file.len());
	out.extend_from_slice(&file[..2]);
	let mut pos: usize = 2;
	let mut dropped = false;

	loop {
		let marker = *file.get(pos.checked_add(1)?)?;
		if *file.get(pos)? != 0xFF {
			return None;
		}

		// Start of scan: the rest is image data, copied whole.
		if marker == 0xDA {
			out.extend_from_slice(file.get(pos..)?);
			break;
		}

		// Markers without a length.
		if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
			out.extend_from_slice(file.get(pos..pos.checked_add(2)?)?);
			pos = pos.checked_add(2)?;
			continue;
		}

		let len_bytes = file.get(pos.checked_add(2)?..pos.checked_add(4)?)?;
		let len = usize::from(u16::from_be_bytes([len_bytes[0], len_bytes[1]]));
		if len < 2 {
			return None;
		}

		let end = pos.checked_add(2)?.checked_add(len)?;
		let segment = file.get(pos..end)?;
		if matches!(marker, 0xE1 | 0xEC | 0xED | 0xFE) {
			dropped = true;
		} else {
			out.extend_from_slice(segment);
		}

		pos = end;
	}

	dropped.then_some(out)
}

/// PNG: drops eXIf and the text chunks (tEXt, zTXt, iTXt), which hold EXIF,
/// XMP, comments and the software that made it. Every chunk is checked to be
/// complete before anything is dropped.
fn png(file: &[u8]) -> Option<Vec<u8>> {
	let mut out = Vec::with_capacity(file.len());
	out.extend_from_slice(&file[..8]);
	let mut pos: usize = 8;
	let mut dropped = false;

	while pos < file.len() {
		let header = file.get(pos..pos.checked_add(8)?)?;
		let len = usize::try_from(u32::from_be_bytes([header[0], header[1], header[2], header[3]])).ok()?;
		let kind = &header[4..8];
		// Length, type, data, CRC.
		let end = pos.checked_add(12)?.checked_add(len)?;
		let chunk = file.get(pos..end)?;

		if matches!(kind, b"eXIf" | b"tEXt" | b"zTXt" | b"iTXt") {
			dropped = true;
		} else {
			out.extend_from_slice(chunk);
		}

		pos = end;
		if kind == b"IEND" {
			break;
		}
	}

	dropped.then_some(out)
}

/// WebP: drops the EXIF and XMP chunks and clears their flags in VP8X.
fn webp(file: &[u8]) -> Option<Vec<u8>> {
	let riff_len = usize::try_from(u32::from_le_bytes(file.get(4..8)?.try_into().ok()?)).ok()?;
	let body_end = riff_len.checked_add(8)?;
	if body_end > file.len() {
		return None;
	}

	let mut out = Vec::with_capacity(file.len());
	out.extend_from_slice(&file[..12]);
	let mut pos: usize = 12;
	let mut dropped = false;

	while pos < body_end {
		let header = file.get(pos..pos.checked_add(8)?)?;
		let kind = &header[..4];
		let len = usize::try_from(u32::from_le_bytes(header[4..8].try_into().ok()?)).ok()?;
		// Chunks are padded to an even length.
		let padded = len.checked_add(len & 1)?;
		let end = pos.checked_add(8)?.checked_add(padded)?;
		let chunk = file.get(pos..end.min(body_end))?;

		if matches!(kind, b"EXIF" | b"XMP ") {
			dropped = true;
		} else {
			out.extend_from_slice(chunk);
		}

		pos = end;
	}

	if !dropped {
		return None;
	}

	// VP8X flags: bit 3 EXIF, bit 2 XMP.
	if out.get(12..16) == Some(b"VP8X") {
		if let Some(flags) = out.get_mut(20) {
			*flags &= !0b0000_1100;
		}
	}

	let new_len = u32::try_from(out.len().checked_sub(8)?).ok()?;
	out[4..8].copy_from_slice(&new_len.to_le_bytes());

	Some(out)
}

#[cfg(test)]
mod tests {
	use super::strip_metadata;

	fn jpeg_segment(marker: u8, payload: &[u8]) -> Vec<u8> {
		let len = u16::try_from(payload.len() + 2).unwrap();
		let mut s = vec![0xFF, marker];
		s.extend_from_slice(&len.to_be_bytes());
		s.extend_from_slice(payload);
		s
	}

	#[test]
	fn jpeg_loses_exif_and_comments_keeps_the_rest() {
		let jfif = jpeg_segment(0xE0, b"JFIF\0\x01\x01");
		let icc = jpeg_segment(0xE2, b"ICC_PROFILE\0data");
		let scan = [0xFF, 0xDA, 0x00, 0x02, 1, 2, 3, 0xFF, 0xD9];
		let mut file = vec![0xFF, 0xD8];
		file.extend(&jfif);
		file.extend(jpeg_segment(0xE1, b"Exif\0\0GPS 51.5N 0.1W"));
		file.extend(&icc);
		file.extend(jpeg_segment(0xFE, b"taken at home"));
		file.extend_from_slice(&scan);

		let mut expected = vec![0xFF, 0xD8];
		expected.extend(&jfif);
		expected.extend(&icc);
		expected.extend_from_slice(&scan);

		assert_eq!(strip_metadata(&file), Some(expected));
	}

	#[test]
	fn clean_or_broken_files_are_left_alone() {
		let mut clean = vec![0xFF, 0xD8];
		clean.extend(jpeg_segment(0xE0, b"JFIF\0"));
		clean.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02, 9, 0xFF, 0xD9]);
		assert_eq!(strip_metadata(&clean), None);

		let truncated = [0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x40, b'E'];
		assert_eq!(strip_metadata(&truncated), None);

		assert_eq!(strip_metadata(b"not an image at all"), None);
		assert_eq!(strip_metadata(&[]), None);
	}

	fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
		let mut c = u32::try_from(data.len()).unwrap().to_be_bytes().to_vec();
		c.extend_from_slice(kind);
		c.extend_from_slice(data);
		c.extend_from_slice(&[0, 0, 0, 0]);
		c
	}

	#[test]
	fn png_loses_exif_and_text() {
		let sig = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
		let ihdr = png_chunk(b"IHDR", &[0; 13]);
		let idat = png_chunk(b"IDAT", &[1, 2, 3]);
		let iend = png_chunk(b"IEND", &[]);
		let mut file = sig.to_vec();
		file.extend(&ihdr);
		file.extend(png_chunk(b"eXIf", b"MM\0*GPS"));
		file.extend(png_chunk(b"tEXt", b"Author\0me"));
		file.extend(&idat);
		file.extend(&iend);

		let mut expected = sig.to_vec();
		expected.extend(&ihdr);
		expected.extend(&idat);
		expected.extend(&iend);

		assert_eq!(strip_metadata(&file), Some(expected));
	}

	fn webp_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
		let mut c = kind.to_vec();
		c.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
		c.extend_from_slice(data);
		if data.len() % 2 == 1 {
			c.push(0);
		}
		c
	}

	fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
		let body: Vec<u8> = chunks.concat();
		let mut f = b"RIFF".to_vec();
		f.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_le_bytes());
		f.extend_from_slice(b"WEBP");
		f.extend(body);
		f
	}

	#[test]
	fn webp_loses_exif_and_xmp_and_their_flags() {
		let mut vp8x = vec![0b0000_1100, 0, 0, 0];
		vp8x.extend_from_slice(&[0; 6]);
		let image = webp_chunk(b"VP8 ", &[1, 2, 3]);
		let file = riff(&[
			webp_chunk(b"VP8X", &vp8x),
			image.clone(),
			webp_chunk(b"EXIF", b"GPS"),
			webp_chunk(b"XMP ", b"<x/>"),
		]);

		let mut clean_vp8x = vp8x.clone();
		clean_vp8x[0] = 0;
		let expected = riff(&[webp_chunk(b"VP8X", &clean_vp8x), image]);

		assert_eq!(strip_metadata(&file), Some(expected));
	}
}
