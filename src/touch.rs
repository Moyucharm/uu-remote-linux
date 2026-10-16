//! Native touch input (viewer 原生触控 mode) → kernel multitouch contacts.
//!
//! Wire shape observed from the iPad (platform 3) and Android (platform 2) viewers, CONTROL Message
//! field 20 = { 1 TouchEvent { 1 type, 2 repeated Finger { 1 id, 2 x, 3 y, 4 f32, 5 f32 } } }:
//! type 1 = first finger down, 2 = move, 3 = all fingers up (no fingers). x/y are 0..1 of the
//! desktop (corner taps gave 0.01..0.99); fields 4/5 were constant 4.0 and are ignored.
//! A second finger has no own "down": it simply appears in the next move, and a lifted finger
//! disappears from the list. List order is not stable, so contacts are matched by finger id.
//! Moves with an empty list also occur and carry no information.

use crate::protocol::{decode_wire, WireValue};

pub const MAX_CONTACTS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Finger { pub id: u64, pub x: f32, pub y: f32 }

#[derive(Debug, Clone, PartialEq)]
pub enum TouchEvent { Fingers(Vec<Finger>), Up }

/// Decodes the field-20 payload; None for anything that is not a well-formed touch event.
pub fn decode(payload: &[u8]) -> Option<TouchEvent> {
    let outer = decode_wire(payload).ok()?;
    let event = outer.iter().find_map(|f| match (f.number, &f.value) { (1, WireValue::Bytes(b)) => Some(*b), _ => None })?;
    let fields = decode_wire(event).ok()?;
    let kind = fields.iter().find_map(|f| match (f.number, &f.value) { (1, WireValue::Varint(v)) => Some(*v), _ => None })?;
    if kind == 3 { return Some(TouchEvent::Up); }
    if kind != 1 && kind != 2 { return None; }
    let mut fingers = Vec::new();
    for field in &fields {
        let (2, WireValue::Bytes(b)) = (field.number, &field.value) else { continue };
        let inner = decode_wire(b).ok()?;
        let id = inner.iter().find_map(|x| match (x.number, &x.value) { (1, WireValue::Varint(v)) => Some(*v), _ => None })?;
        let float = |n: u32| inner.iter().find_map(|x| match (x.number, &x.value) { (n2, WireValue::Fixed32(raw)) if n2 == n => Some(f32::from_le_bytes(*raw)), _ => None });
        let (x, y) = (float(2)?, float(3)?);
        if !x.is_finite() || !y.is_finite() { return None; }
        if !fingers.iter().any(|f: &Finger| f.id == id) { fingers.push(Finger { id, x: x.clamp(0.0, 1.0), y: y.clamp(0.0, 1.0) }); }
    }
    Some(TouchEvent::Fingers(fingers))
}

/// One kernel slot change; positions are 0..=ABS_MAX.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contact { Down { slot: usize, tracking: i32, x: i32, y: i32 }, Move { slot: usize, x: i32, y: i32 }, Up { slot: usize } }

/// Maps viewer finger ids to kernel slots across messages.
#[derive(Default)]
pub struct Tracker { slots: [Option<u64>; MAX_CONTACTS], next_tracking: i32 }

impl Tracker {
    pub fn active(&self) -> bool { self.slots.iter().any(Option::is_some) }

    pub fn update(&mut self, event: &TouchEvent) -> Vec<Contact> {
        let fingers: &[Finger] = match event {
            TouchEvent::Up => &[],
            TouchEvent::Fingers(f) if f.is_empty() => return Vec::new(), // empty move: no information
            TouchEvent::Fingers(f) => f,
        };
        let mut changes = Vec::new();
        for (slot, id) in self.slots.iter_mut().enumerate() {
            if id.is_some_and(|id| !fingers.iter().any(|f| f.id == id)) { *id = None; changes.push(Contact::Up { slot }); }
        }
        let scale = |v: f32| (v * crate::uinput::ABS_MAX as f32).round() as i32;
        for finger in fingers {
            let (x, y) = (scale(finger.x), scale(finger.y));
            if let Some(slot) = self.slots.iter().position(|s| *s == Some(finger.id)) {
                changes.push(Contact::Move { slot, x, y });
            } else if let Some(slot) = self.slots.iter().position(Option::is_none) {
                self.slots[slot] = Some(finger.id);
                self.next_tracking = (self.next_tracking + 1) & 0xffff;
                changes.push(Contact::Down { slot, tracking: self.next_tracking, x, y });
            }
        }
        changes
    }

    pub fn reset(&mut self) { self.slots = [None; MAX_CONTACTS]; }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(mut v: u64, out: &mut Vec<u8>) { while v >= 128 { out.push((v as u8 & 127) | 128); v >>= 7; } out.push(v as u8); }
    fn bytes(field: u64, body: &[u8], out: &mut Vec<u8>) { varint(field << 3 | 2, out); varint(body.len() as u64, out); out.extend_from_slice(body); }
    fn finger(id: u64, x: f32, y: f32) -> Vec<u8> {
        let mut out = Vec::new();
        varint(1 << 3, &mut out); varint(id, &mut out);
        for (n, v) in [(2u64, x), (3, y), (4, 4.0), (5, 4.0)] { varint(n << 3 | 5, &mut out); out.extend_from_slice(&v.to_le_bytes()); }
        out
    }
    fn event(kind: u64, fingers: &[Vec<u8>]) -> Vec<u8> {
        let mut inner = Vec::new();
        varint(1 << 3, &mut inner); varint(kind, &mut inner);
        for f in fingers { bytes(2, f, &mut inner); }
        let mut out = Vec::new();
        bytes(1, &inner, &mut out);
        out
    }

    #[test]
    fn decodes_observed_shapes() {
        let id = 0x1234_5678_9abc;
        assert_eq!(decode(&event(1, &[finger(id, 0.01, 0.99)])), Some(TouchEvent::Fingers(vec![Finger { id, x: 0.01, y: 0.99 }])));
        // "type 3 without fingers" encodes to 4 bytes, as the Android viewer's 4-byte message.
        assert_eq!(event(3, &[]).len(), 4);
        assert_eq!(decode(&event(3, &[])), Some(TouchEvent::Up));
        assert_eq!(decode(&event(2, &[])), Some(TouchEvent::Fingers(vec![])));
        assert_eq!(decode(&event(9, &[])), None);
        assert_eq!(decode(&event(2, &[finger(1, f32::NAN, 0.5)])), None);
        assert_eq!(decode(&[0xff]), None);
    }

    #[test]
    fn second_finger_joins_and_leaves_by_id_regardless_of_order() {
        let mut t = Tracker::default();
        let f = |id, x| Finger { id, x, y: 0.5 };
        assert!(matches!(t.update(&TouchEvent::Fingers(vec![f(7, 0.0)]))[..], [Contact::Down { slot: 0, .. }]));
        // Second finger appears first in the list: finger 7 must stay in slot 0.
        let c = t.update(&TouchEvent::Fingers(vec![f(9, 1.0), f(7, 0.5)]));
        assert_eq!(c, vec![Contact::Down { slot: 1, tracking: 2, x: 65535, y: 32768 }, Contact::Move { slot: 0, x: 32768, y: 32768 }]);
        // Empty move changes nothing.
        assert!(t.update(&TouchEvent::Fingers(vec![])).is_empty());
        // Finger 7 lifted first.
        assert_eq!(t.update(&TouchEvent::Fingers(vec![f(9, 1.0)])), vec![Contact::Up { slot: 0 }, Contact::Move { slot: 1, x: 65535, y: 32768 }]);
        assert_eq!(t.update(&TouchEvent::Up), vec![Contact::Up { slot: 1 }]);
        assert!(!t.active());
    }

    #[test]
    fn caps_contacts_and_reset_clears() {
        let mut t = Tracker::default();
        let fingers: Vec<Finger> = (0..12).map(|id| Finger { id, x: 0.0, y: 0.0 }).collect();
        assert_eq!(t.update(&TouchEvent::Fingers(fingers)).len(), MAX_CONTACTS);
        t.reset();
        assert!(!t.active());
    }
}
