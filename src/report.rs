//! Decode a PTP touch input report (ID 0x01) using a [`Layout`] from
//! [`crate::descriptor::parse`]. Coordinates are converted from chip
//! pixels to millimeters using the descriptor's per-axis density
//! ([`Layout::mm_per_logical_px_x`] / `_y`) so downstream gesture code
//! works in physical units and is firmware-agnostic.

use crate::descriptor::Layout;

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub struct Contact {
    pub id: u8,
    /// X position in millimeters (left → right).
    pub x: f64,
    /// Y position in millimeters (top → bottom; PTP origin is top-left).
    pub y: f64,
    pub tip: bool,
    pub confidence: bool,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Frame {
    pub contacts: Vec<Contact>,
    pub scan_time_100us: u16,
    pub button: bool,
}

/// Extract `count` bits starting at `bit_offset`, LSB-first within each
/// byte and least-significant byte first — the packing HID uses.
///
/// Reproduces the previous hand-written 5- and 6-byte decoders exactly;
/// those layouts are just two of the shapes this handles.
pub(crate) fn read_bits(buf: &[u8], bit_offset: usize, count: usize) -> u32 {
    let mut value: u32 = 0;
    for i in 0..count.min(32) {
        let bit = bit_offset + i;
        let byte = match buf.get(bit / 8) {
            Some(b) => *b,
            None => break,
        };
        if (byte >> (bit % 8)) & 1 != 0 {
            value |= 1 << i;
        }
    }
    value
}

/// Decode one packet. Production consumers must use [`FrameAssembler`] to
/// handle devices which split one scan across multiple input reports.
pub fn decode(layout: &Layout, report: &[u8]) -> Option<Frame> {
    decode_contacts(layout, report, None)
}

fn decode_contacts(layout: &Layout, report: &[u8], count: Option<usize>) -> Option<Frame> {
    // Layout is public and can be constructed without parse(). Verify
    // every bound before indexing even for externally supplied layouts.
    layout.validate().ok()?;
    if report.len() < layout.total_payload_bytes {
        return None;
    }
    if report[0] != layout.report_id {
        return None;
    }

    let contact_count = report[layout.contact_count_offset] as usize;
    let n = count.unwrap_or(contact_count).min(layout.contact_slots);

    let mm_per_px_x = layout.mm_per_logical_px_x();
    let mm_per_px_y = layout.mm_per_logical_px_y();

    let fields = &layout.contact;
    let mut contacts = Vec::with_capacity(n);
    for i in 0..n {
        let base = layout.fingers_bit_offset + i * layout.contact_stride_bits;
        if (base + layout.contact_stride_bits).div_ceil(8) > report.len() {
            break;
        }

        // Read each field where the descriptor said it is, rather than
        // assuming a stride. Anything the device reports in between —
        // Width, Height, Pressure, Azimuth — is simply skipped.
        let id = read_bits(report, base + fields.id.offset, fields.id.size) as u8;
        let x = read_bits(report, base + fields.x.offset, fields.x.size) as i32;
        let y = read_bits(report, base + fields.y.offset, fields.y.size) as i32;
        let tip = read_bits(report, base + fields.tip.offset, fields.tip.size) != 0;
        // Legacy descriptors may omit confidence. Preserve compatibility
        // by treating those contacts as intentional.
        let confidence = match fields.confidence {
            Some(f) => read_bits(report, base + f.offset, f.size) != 0,
            None => true,
        };

        contacts.push(Contact {
            id,
            x: (x as f64) * mm_per_px_x,
            y: (y as f64) * mm_per_px_y,
            tip,
            confidence,
        });
    }

    let scan_time = u16::from_le_bytes([
        report[layout.scan_time_offset],
        report[layout.scan_time_offset + 1],
    ]);
    let button = (report[layout.button_offset] & (1 << layout.button_bit)) != 0;

    Some(Frame {
        contacts,
        scan_time_100us: scan_time,
        button,
    })
}

/// Per-device, bounded assembly of PTP parallel and hybrid reporting.
/// Partial frames never reach the gesture engine. A newer first report
/// replaces an incomplete older frame without mixing scans or contact IDs.
pub struct FrameAssembler {
    maximum: usize,
    pending: Option<(usize, Frame)>,
    finished_scan: Option<u16>,
}

impl FrameAssembler {
    pub fn new(maximum: Option<u8>) -> Self {
        Self {
            maximum: maximum.filter(|n| (1..=5).contains(n)).unwrap_or(5) as usize,
            pending: None,
            finished_scan: None,
        }
    }

    pub fn push(&mut self, layout: &Layout, bytes: &[u8]) -> Result<Option<Frame>, &'static str> {
        let result = self.push_inner(layout, bytes);
        if result.is_err() {
            let pending_scan = self.pending.take().map(|(_, f)| f.scan_time_100us);
            // Reject the rest of a malformed frame too; its zero-count
            // continuations must not turn into a fabricated lift.
            self.finished_scan = layout
                .scan_time_offset
                .checked_add(2)
                .and_then(|end| bytes.get(layout.scan_time_offset..end))
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .or(pending_scan);
        }
        result
    }

    fn push_inner(&mut self, layout: &Layout, bytes: &[u8]) -> Result<Option<Frame>, &'static str> {
        // Ignore sibling reports without disturbing a pending touch frame.
        if bytes.first().is_some_and(|id| *id != layout.report_id) {
            return Ok(None);
        }
        let mut frame = decode(layout, bytes).ok_or("invalid touch report")?;
        let count = bytes[layout.contact_count_offset] as usize;
        if count > self.maximum {
            return Err("contact count exceeds device maximum");
        }
        if count > 0 {
            self.pending = None;
            self.finished_scan = None;
            if count > layout.contact_slots {
                self.pending = Some((
                    count,
                    Frame {
                        contacts: Vec::with_capacity(count),
                        scan_time_100us: frame.scan_time_100us,
                        button: frame.button,
                    },
                ));
            }
        } else if let Some((total, pending)) = self.pending.as_ref() {
            if pending.scan_time_100us != frame.scan_time_100us {
                return Err("hybrid continuation changed scan time");
            }
            frame = decode_contacts(layout, bytes, Some(total - pending.contacts.len()))
                .ok_or("invalid hybrid continuation")?;
        } else if self.finished_scan == Some(frame.scan_time_100us) {
            return Ok(None);
        }

        let mut ids = [false; 256];
        if let Some((_, pending)) = &self.pending {
            for contact in &pending.contacts {
                ids[contact.id as usize] = true;
            }
        }
        for contact in &frame.contacts {
            if std::mem::replace(&mut ids[contact.id as usize], true) {
                return Err("duplicate contact ID in frame");
            }
        }
        if let Some((total, pending)) = self.pending.as_mut() {
            if pending.button != frame.button {
                return Err("hybrid button state changed within scan");
            }
            pending.contacts.extend(frame.contacts);
            if pending.contacts.len() < *total {
                return Ok(None);
            }
            let (_, complete) = self.pending.take().unwrap();
            self.finished_scan = Some(complete.scan_time_100us);
            Ok(Some(complete))
        } else {
            Ok(Some(frame))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{BitField, ContactFields};

    #[test]
    fn exact_report_boundary_decodes_but_one_byte_short_does_not() {
        let layout = fake_layout();
        let mut report = vec![0xff; layout.total_payload_bytes];
        report[0] = layout.report_id;
        assert_eq!(
            decode(&layout, &report).unwrap().contacts.len(),
            layout.contact_slots
        );
        for len in 0..report.len() {
            assert!(decode(&layout, &report[..len]).is_none());
        }
    }

    #[test]
    fn manually_constructed_layouts_cannot_index_outside_the_report() {
        let good = fake_layout();
        let mut report = vec![0xff; good.total_payload_bytes];
        report[0] = good.report_id;
        let mutations: &[fn(&mut Layout)] = &[
            |l| l.total_payload_bytes = 0,
            |l| l.total_payload_bytes = usize::MAX,
            |l| l.scan_time_offset = l.total_payload_bytes - 1,
            |l| l.scan_time_offset = usize::MAX,
            |l| l.contact_count_offset = l.total_payload_bytes,
            |l| l.button_offset = l.total_payload_bytes,
            |l| l.button_bit = 8,
            |l| l.contact_slots = usize::MAX,
            |l| l.fingers_bit_offset = usize::MAX,
            |l| l.contact_stride_bits = usize::MAX,
            |l| l.contact.x.offset = usize::MAX,
            |l| l.contact.y.size = 33,
            |l| l.contact.id.size = 9,
            |l| l.contact.tip.size = 0,
        ];
        for mutate in mutations {
            let mut bad = good.clone();
            mutate(&mut bad);
            assert!(decode(&bad, &report).is_none(), "accepted {bad:?}");
        }
    }

    fn fake_layout() -> Layout {
        Layout {
            report_id: 0x01,
            contact_slots: 5,
            bytes_per_contact: 6,
            fingers_offset: 1,
            fingers_bit_offset: 8,
            contact_stride_bits: 48,
            contact: ContactFields {
                confidence: Some(BitField { offset: 0, size: 1 }),
                tip: BitField { offset: 1, size: 1 },
                id: BitField { offset: 8, size: 8 },
                x: BitField {
                    offset: 16,
                    size: 16,
                },
                y: BitField {
                    offset: 32,
                    size: 16,
                },
            },
            scan_time_offset: 31,
            contact_count_offset: 33,
            button_offset: 34,
            button_bit: 0,
            logical_x_max: 3936,
            logical_y_max: 2424,
            physical_x_max_mm: 65.0,
            physical_y_max_mm: 40.0,
            total_payload_bytes: 35,
            contact_count_max: None,
            input_mode_report_id: None,
            selective_reporting_report_id: None,
            latency_mode_report_id: None,
            vendor_feature_report_ids: Vec::new(),
            standard_feature_report_ids: Vec::new(),
        }
    }

    #[test]
    fn decodes_two_contacts() {
        let layout = fake_layout();
        let mut buf = vec![0u8; 35];
        buf[0] = 0x01;
        // Contact 0: tip=1, conf=1, id=7, x=1968, y=1212 (pad midpoint)
        buf[1] = 0x03;
        buf[2] = 7;
        buf[3..5].copy_from_slice(&1968u16.to_le_bytes());
        buf[5..7].copy_from_slice(&1212u16.to_le_bytes());
        // Contact 1: tip=1, conf=1, id=8, x=2952, y=606
        buf[7] = 0x03;
        buf[8] = 8;
        buf[9..11].copy_from_slice(&2952u16.to_le_bytes());
        buf[11..13].copy_from_slice(&606u16.to_le_bytes());
        // scan_time = 0x1234, count=2, button=1
        buf[31..33].copy_from_slice(&0x1234u16.to_le_bytes());
        buf[33] = 2;
        buf[34] = 0x01;

        let frame = decode(&layout, &buf).expect("decode");
        assert_eq!(frame.contacts.len(), 2);
        assert_eq!(frame.contacts[0].id, 7);
        // Midpoint chip pixel → midpoint mm.
        assert!(
            (frame.contacts[0].x - 32.5).abs() < 0.05,
            "{}",
            frame.contacts[0].x
        );
        assert!(
            (frame.contacts[0].y - 20.0).abs() < 0.05,
            "{}",
            frame.contacts[0].y
        );
        assert_eq!(frame.scan_time_100us, 0x1234);
        assert!(frame.button);
    }

    #[test]
    fn decodes_packed_five_byte_contacts() {
        let layout = Layout {
            report_id: 0x1e,
            contact_slots: 5,
            bytes_per_contact: 5,
            fingers_offset: 1,
            fingers_bit_offset: 8,
            contact_stride_bits: 40,
            contact: ContactFields {
                confidence: Some(BitField { offset: 0, size: 1 }),
                tip: BitField { offset: 1, size: 1 },
                id: BitField { offset: 2, size: 6 },
                x: BitField {
                    offset: 8,
                    size: 16,
                },
                y: BitField {
                    offset: 24,
                    size: 16,
                },
            },
            scan_time_offset: 26,
            contact_count_offset: 28,
            button_offset: 29,
            button_bit: 0,
            logical_x_max: 2160,
            logical_y_max: 1600,
            physical_x_max_mm: 209.8,
            physical_y_max_mm: 119.1,
            total_payload_bytes: 30,

            contact_count_max: None,
            input_mode_report_id: Some(0x25),
            selective_reporting_report_id: Some(0x22),
            latency_mode_report_id: Some(0x23),
            vendor_feature_report_ids: Vec::new(),
            standard_feature_report_ids: Vec::new(),
        };

        let mut buf = vec![0u8; 30];

        // Report ID.
        buf[0] = 0x1e;

        // Contact 0:
        // confidence=1, tip=1, id=7
        // packed flags = (7 << 2) | 0b11 = 0x1f
        buf[1] = (7 << 2) | 0x03;

        // X = 1080, Y = 800 (logical midpoint).
        buf[2..4].copy_from_slice(&1080u16.to_le_bytes());
        buf[4..6].copy_from_slice(&800u16.to_le_bytes());

        // Contact 1:
        // confidence=1, tip=1, id=8
        buf[6] = (8 << 2) | 0x03;
        buf[7..9].copy_from_slice(&1620u16.to_le_bytes());
        buf[9..11].copy_from_slice(&400u16.to_le_bytes());

        // Trailing PTP fields.
        buf[26..28].copy_from_slice(&0x1234u16.to_le_bytes());
        buf[28] = 2; // contact count
        buf[29] = 0x01; // button

        let frame = decode(&layout, &buf).expect("decode");

        assert_eq!(frame.contacts.len(), 2);

        assert_eq!(frame.contacts[0].id, 7);
        assert!(frame.contacts[0].tip);
        assert!(frame.contacts[0].confidence);

        // Logical midpoint should map to physical midpoint.
        assert!(
            (frame.contacts[0].x - 104.9).abs() < 0.1,
            "{}",
            frame.contacts[0].x
        );
        assert!(
            (frame.contacts[0].y - 59.55).abs() < 0.1,
            "{}",
            frame.contacts[0].y
        );

        assert_eq!(frame.contacts[1].id, 8);
        assert!(frame.contacts[1].tip);
        assert!(frame.contacts[1].confidence);

        assert_eq!(frame.scan_time_100us, 0x1234);
        assert!(frame.button);
    }
    fn packet(layout: &Layout, count: u8, scan: u16, ids: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0; layout.total_payload_bytes];
        bytes[0] = layout.report_id;
        bytes[layout.contact_count_offset] = count;
        bytes[layout.scan_time_offset..layout.scan_time_offset + 2]
            .copy_from_slice(&scan.to_le_bytes());
        for (slot, id) in ids.iter().enumerate() {
            let base = layout.fingers_offset + slot * layout.bytes_per_contact;
            bytes[base] = 3;
            bytes[base + 1] = *id;
        }
        bytes
    }

    #[test]
    fn hybrid_frames_include_all_contacts_and_ignore_unused_final_slots() {
        for slots in [1, 2] {
            let mut layout = fake_layout();
            layout.contact_slots = slots;
            let mut decoder = FrameAssembler::new(Some(5));
            let ids = [255, 0, 128, 7, 9];
            let mut result = None;
            for (index, chunk) in ids.chunks(slots).enumerate() {
                let mut padded = chunk.to_vec();
                padded.resize(slots, 255); // Invalid duplicate must be ignored padding.
                result = decoder
                    .push(
                        &layout,
                        &packet(&layout, if index == 0 { 5 } else { 0 }, 42, &padded),
                    )
                    .unwrap();
                if (index + 1) * slots < ids.len() {
                    assert!(result.is_none());
                }
            }
            let frame = result.unwrap();
            assert_eq!(frame.contacts.iter().map(|c| c.id).collect::<Vec<_>>(), ids);
            // A duplicate continuation must not fabricate a lift.
            assert!(
                decoder
                    .push(&layout, &packet(&layout, 0, 42, &[9]))
                    .unwrap()
                    .is_none()
            );
            let lift = decoder
                .push(&layout, &packet(&layout, 0, 43, &[]))
                .unwrap()
                .unwrap();
            assert!(lift.contacts.is_empty());
        }
    }

    #[test]
    fn hybrid_recovers_from_missing_frames_bad_ids_and_scan_wrap() {
        let mut layout = fake_layout();
        layout.contact_slots = 1;
        let mut decoder = FrameAssembler::new(Some(4));
        assert!(
            decoder
                .push(&layout, &packet(&layout, 2, 65535, &[1]))
                .unwrap()
                .is_none()
        );
        // A sibling report cannot destroy the pending touch frame.
        assert!(decoder.push(&layout, &[99]).unwrap().is_none());
        assert!(
            decoder
                .push(&layout, &packet(&layout, 0, 65535, &[1]))
                .is_err()
        );
        assert!(
            decoder
                .push(&layout, &packet(&layout, 0, 65535, &[2]))
                .unwrap()
                .is_none()
        );
        assert!(
            decoder
                .push(&layout, &packet(&layout, 2, 0, &[3]))
                .unwrap()
                .is_none()
        );
        assert!(decoder.push(&layout, &packet(&layout, 0, 1, &[4])).is_err());
        // A new first report supersedes an incomplete scan.
        assert!(
            decoder
                .push(&layout, &packet(&layout, 2, 2, &[5]))
                .unwrap()
                .is_none()
        );
        assert!(
            decoder
                .push(&layout, &packet(&layout, 2, 3, &[6]))
                .unwrap()
                .is_none()
        );
        let frame = decoder
            .push(&layout, &packet(&layout, 0, 3, &[7]))
            .unwrap()
            .unwrap();
        assert_eq!(
            frame.contacts.iter().map(|c| c.id).collect::<Vec<_>>(),
            [6, 7]
        );
        assert!(decoder.push(&layout, &packet(&layout, 5, 4, &[1])).is_err());
        assert!(
            decoder
                .push(&layout, &packet(&layout, 0, 4, &[2]))
                .unwrap()
                .is_none()
        );
        assert!(decoder.push(&layout, &[layout.report_id]).is_err());
    }

    #[test]
    fn parallel_frames_preserve_button_only_reports_and_reject_duplicate_ids() {
        let layout = fake_layout();
        let mut decoder = FrameAssembler::new(None);
        let mut bytes = packet(&layout, 0, 100, &[]);
        bytes[layout.button_offset] = 1;
        assert!(decoder.push(&layout, &bytes).unwrap().unwrap().button);
        assert_eq!(
            decoder
                .push(&layout, &packet(&layout, 4, 101, &[1, 2, 3, 4]))
                .unwrap()
                .unwrap()
                .contacts
                .len(),
            4
        );
        assert!(
            decoder
                .push(&layout, &packet(&layout, 2, 102, &[1, 1]))
                .is_err()
        );
    }
}
