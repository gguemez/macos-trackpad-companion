//! macOS 27 DockSwipe IOHID wire representation.
//!
//! Layout: Apple's IOHIDEventData.h (queue header, swipe and axis records).
//! CGEvent field 4205 augmentation: https://github.com/joshuarli/iss (0BSD).
//! Encode explicitly to avoid Rust struct padding or uninitialised bytes.

const RAW_IOHID_FIELD: u16 = 4205;
const QUEUE_SIZE: usize = 28;
const SWIPE_SIZE: usize = 40;
const VELOCITY_SIZE: usize = 28;

pub(crate) struct DockSwipe {
    pub motion: u16,
    pub phase: u32,
    pub progress: f64,
    pub velocity: Option<f64>,
    pub timestamp_ns: u64,
}

fn fixed(value: f64) -> i32 {
    // Q16.16. Saturate large values and retain the sign of tiny motion.
    let encoded = (value * 65536.0) as i32;
    if encoded == 0 && value.is_finite() && value != 0.0 {
        if value > 0.0 { 1 } else { -1 }
    } else {
        encoded
    }
}

impl DockSwipe {
    pub fn payload(&self) -> Vec<u8> {
        let velocity_size = self.velocity.map_or(0, |_| VELOCITY_SIZE);
        let mut data = vec![0; QUEUE_SIZE + SWIPE_SIZE + velocity_size];
        data[0..8].copy_from_slice(&self.timestamp_ns.to_le_bytes());
        // senderID, queue options, attributeLength remain zero.
        data[24..28]
            .copy_from_slice(&(if self.velocity.is_some() { 2u32 } else { 1 }).to_le_bytes());

        let swipe = &mut data[QUEUE_SIZE..QUEUE_SIZE + SWIPE_SIZE];
        swipe[0..4].copy_from_slice(&(SWIPE_SIZE as u32).to_le_bytes());
        swipe[4..8].copy_from_slice(&23u32.to_le_bytes()); // DockSwipe
        swipe[8..12].copy_from_slice(&((self.phase & 0xff) << 24).to_le_bytes());
        // depth, position, legacy swipe mask remain zero.
        swipe[32..34].copy_from_slice(&self.motion.to_le_bytes());
        swipe[34..36].copy_from_slice(&3u16.to_le_bytes()); // DockPrimary flavor
        swipe[36..40].copy_from_slice(&fixed(self.progress).to_le_bytes());

        if let Some(v) = self.velocity {
            let velocity = &mut data[QUEUE_SIZE + SWIPE_SIZE..];
            velocity[0..4].copy_from_slice(&(VELOCITY_SIZE as u32).to_le_bytes());
            velocity[4..8].copy_from_slice(&9u32.to_le_bytes()); // Velocity
            velocity[12] = 1; // child of the swipe, not a second root
            velocity[16..20].copy_from_slice(&fixed(v).to_le_bytes());
            velocity[20..24].copy_from_slice(&fixed(v).to_le_bytes());
        }
        data
    }

    pub fn append_to(&self, serialized_event: &mut Vec<u8>) -> Result<(), &'static str> {
        if !serialized_event.starts_with(&2u32.to_be_bytes()) {
            return Err("unsupported CGEvent serialization version");
        }
        let payload = self.payload();
        serialized_event.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        serialized_event.extend_from_slice(&RAW_IOHID_FIELD.to_be_bytes());
        serialized_event.extend_from_slice(&payload);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(data: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
    }

    #[test]
    fn both_axes_and_all_phases_have_valid_record_boundaries() {
        for motion in [1, 2] {
            for phase in [1, 2, 4, 8] {
                for progress in [-0.25, 0.25] {
                    let terminal = phase == 4 || phase == 8;
                    let p = DockSwipe {
                        motion,
                        phase,
                        progress,
                        velocity: terminal.then_some(-3.5),
                        timestamp_ns: 123456789,
                    }
                    .payload();
                    assert_eq!(u64::from_le_bytes(p[..8].try_into().unwrap()), 123456789);
                    assert_eq!(u32_at(&p, 20), 0); // attributes length
                    assert_eq!(u32_at(&p, 24), if terminal { 2 } else { 1 });
                    assert_eq!(u32_at(&p, 28), 40);
                    assert_eq!(u32_at(&p, 32), 23);
                    assert_eq!(u32_at(&p, 36), phase << 24);
                    assert_eq!(&p[60..64], &[motion as u8, 0, 3, 0]);
                    assert_eq!(u32_at(&p, 64) as i32, (progress * 65536.0) as i32);
                    if terminal {
                        assert_eq!(p.len(), 96);
                        assert_eq!(u32_at(&p, 68), 28);
                        assert_eq!(u32_at(&p, 72), 9);
                        assert_eq!(&p[80..84], &[1, 0, 0, 0]);
                        assert_eq!(u32_at(&p, 84) as i32, -229376);
                        assert_eq!(u32_at(&p, 88) as i32, -229376);
                        assert_eq!(u32_at(&p, 92), 0);
                    } else {
                        assert_eq!(p.len(), 68);
                    }
                }
            }
        }
    }

    #[test]
    fn field_header_is_big_endian_and_base_event_is_preserved() {
        let swipe = DockSwipe {
            motion: 1,
            phase: 1,
            progress: 0.1,
            velocity: None,
            timestamp_ns: 1,
        };
        let base = vec![0, 0, 0, 2, 0xab, 0xcd];
        let mut bytes = base.clone();
        swipe.append_to(&mut bytes).unwrap();
        assert_eq!(&bytes[..base.len()], &base);
        assert_eq!(&bytes[base.len()..base.len() + 4], &[0, 68, 0x10, 0x6d]);
        assert!(swipe.append_to(&mut vec![0, 0, 0, 3]).is_err());
    }

    #[test]
    fn fixed_point_preserves_tiny_motion_and_saturates() {
        assert_eq!(fixed(0.0), 0);
        assert_eq!(fixed(1e-9), 1);
        assert_eq!(fixed(-1e-9), -1);
        assert_eq!(fixed(1e9), i32::MAX);
        assert_eq!(fixed(-1e9), i32::MIN);
    }
}
