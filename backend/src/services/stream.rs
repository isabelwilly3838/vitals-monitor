//! Real-time fan-out of vitals samples.
//!
//! Default WebSocket encoding is a compact **binary frame** (28 bytes vs ~1.6 kB for the two
//! equivalent FHIR Observations). Clients may request `fhir` encoding instead.
//!
//! Binary frame layout (little-endian, 28 bytes):
//! ```text
//! offset size type     field
//! 0      1    u8       version (=1)
//! 1      1    u8       patient index (into the patient list sent in the hello message)
//! 2      4    u32      sequence number (per patient)
//! 6      8    f64      timestamp, ms since Unix epoch
//! 14     4    f32      body temperature, Cel          (NaN = not measured)
//! 18     4    f32      systolic, mm[Hg]               (NaN = not measured)
//! 22     4    f32      diastolic, mm[Hg]              (NaN = not measured)
//! 26     1    u8       alert flags (bit0 fever, bit1 hypothermia, bit2 hypertension,
//!                                   bit3 hypotension, bit4 rapid change)
//! 27     1    u8       reserved
//! ```

use tokio::sync::broadcast;

pub const FRAME_BYTES: usize = 28;
pub const FRAME_VERSION: u8 = 1;

pub const ALERT_FEVER: u8 = 1;
pub const ALERT_HYPOTHERMIA: u8 = 2;
pub const ALERT_HYPERTENSION: u8 = 4;
pub const ALERT_HYPOTENSION: u8 = 8;
pub const ALERT_RAPID_CHANGE: u8 = 16;

#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub patient_id: String,
    pub patient_index: u8,
    pub seq: u32,
    pub ts_ms: f64,
    pub temp_c: f32,
    pub systolic: f32,
    pub diastolic: f32,
    pub alerts: u8,
}

pub fn encode(s: &Sample) -> [u8; FRAME_BYTES] {
    let mut b = [0u8; FRAME_BYTES];
    b[0] = FRAME_VERSION;
    b[1] = s.patient_index;
    b[2..6].copy_from_slice(&s.seq.to_le_bytes());
    b[6..14].copy_from_slice(&s.ts_ms.to_le_bytes());
    b[14..18].copy_from_slice(&s.temp_c.to_le_bytes());
    b[18..22].copy_from_slice(&s.systolic.to_le_bytes());
    b[22..26].copy_from_slice(&s.diastolic.to_le_bytes());
    b[26] = s.alerts;
    b
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum FrameError {
    #[error("frame must be {FRAME_BYTES} bytes")]
    Length,
    #[error("unsupported frame version")]
    Version,
    #[error("unknown patient index")]
    Patient,
}

pub fn decode(buf: &[u8], patient_ids: &[String]) -> Result<Sample, FrameError> {
    if buf.len() != FRAME_BYTES {
        return Err(FrameError::Length);
    }
    if buf[0] != FRAME_VERSION {
        return Err(FrameError::Version);
    }
    let idx = buf[1];
    let patient_id = patient_ids
        .get(idx as usize)
        .ok_or(FrameError::Patient)?
        .clone();
    let f32_at = |o: usize| f32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
    Ok(Sample {
        patient_id,
        patient_index: idx,
        seq: u32::from_le_bytes(buf[2..6].try_into().unwrap()),
        ts_ms: f64::from_le_bytes(buf[6..14].try_into().unwrap()),
        temp_c: f32_at(14),
        systolic: f32_at(18),
        diastolic: f32_at(22),
        alerts: buf[26],
    })
}

/// Pub/sub hub on a bounded broadcast channel. A slow subscriber *lags* (loses its oldest
/// frames) instead of blocking the producer or growing memory: per-client fault isolation.
pub struct Hub {
    tx: broadcast::Sender<Sample>,
}

impl Default for Hub {
    fn default() -> Self {
        Self::new(256)
    }
}

impl Hub {
    pub fn new(capacity: usize) -> Self {
        Self {
            tx: broadcast::channel(capacity).0,
        }
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Sample> {
        self.tx.subscribe()
    }
    pub fn publish(&self, s: Sample) {
        let _ = self.tx.send(s); // Err only means "no subscribers"
    }
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(seq: u32) -> Sample {
        Sample {
            patient_id: "P001".into(),
            patient_index: 0,
            seq,
            ts_ms: 1.5e12,
            temp_c: 37.2,
            systolic: 120.0,
            diastolic: 80.0,
            alerts: ALERT_FEVER,
        }
    }

    #[test]
    fn frame_is_28_bytes_and_roundtrips() {
        let ids = vec!["P001".to_string()];
        let s = sample(7);
        let b = encode(&s);
        assert_eq!(b.len(), 28);
        assert_eq!(decode(&b, &ids).unwrap(), s);
    }

    #[test]
    fn decode_rejects_bad_frames() {
        let ids = vec!["P001".to_string()];
        assert_eq!(decode(&[0; 5], &ids), Err(FrameError::Length));
        let mut b = encode(&sample(1));
        b[0] = 9;
        assert_eq!(decode(&b, &ids), Err(FrameError::Version));
        let mut b = encode(&sample(1));
        b[1] = 3;
        assert_eq!(decode(&b, &ids), Err(FrameError::Patient));
    }

    #[tokio::test]
    async fn slow_subscriber_lags_instead_of_blocking() {
        let hub = Hub::new(2);
        let mut rx = hub.subscribe();
        for i in 0..5 {
            hub.publish(sample(i)); // never blocks
        }
        assert!(matches!(
            rx.recv().await,
            Err(broadcast::error::RecvError::Lagged(3))
        ));
        assert_eq!(rx.recv().await.unwrap().seq, 3);
    }
}
