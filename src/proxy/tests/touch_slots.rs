//! Wire-level lifetime regressions: no host devices or compositor required.
use super::*;

fn tracking_changes(events: &[(u16, u16, i32)]) -> Vec<(i32, i32)> {
    let mut slot = 0;
    let mut changes = Vec::new();
    for &(ty, code, value) in events {
        if ty == EV_ABS && code == ABS_MT_SLOT {
            slot = value;
        } else if ty == EV_ABS && code == ABS_MT_TRACKING_ID {
            changes.push((slot, value));
        }
    }
    changes
}

#[test]
fn hovering_tappers_never_restart_the_resting_finger() {
    // The recorded failure: [7] -> [6 hover, 7] -> [5 hover, 6 hover,
    // 7, 10 hover]. Positional slots turned the held finger into a 10 ms
    // tap before the newcomers even touched down. Cover 2 and 3 tappers,
    // their activation, report reordering, liftoff and disappearing blocks.
    for extras in [&[4, 6][..], &[6, 5, 10][..]] {
        let held = finger(7, 9, 100, 200);
        let mut contacts = vec![held];
        let mut frames = vec![contacts.clone()];
        for &id in extras {
            let mut hover = finger(id, 0, i32::from(id) * 100, 0);
            hover.contact = false;
            contacts.push(hover);
            contacts.sort_by_key(|f| f.id);
            frames.push(contacts.clone());
        }
        for f in &mut contacts {
            f.contact = true;
        }
        frames.push(contacts.clone());
        contacts.reverse();
        frames.push(contacts.clone());
        for f in &mut contacts {
            if f.id != 7 {
                f.contact = false;
            }
        }
        frames.push(contacts.clone());
        for &id in extras {
            contacts.retain(|f| f.id != id);
            frames.push(contacts.clone());
        }
        let n = frames.len();
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let script = frames.into_iter().map(|f| Ok(Some(frame(f, 0)))).collect();
        let mut px = drive_ready_proxy(&sinks, &haptic, script);
        let cfg = Config::default();
        let t0 = std::time::Instant::now();
        let mut starts = 0;
        for i in 0..n {
            let mark = wire_events(&sinks.pointer).len();
            // The resting touch has exceeded libinput's 180 ms timeout;
            // successive hovering arrivals are then just 10 ms apart.
            let ms = if i == 0 { 0 } else { 500 + (i as u64 - 1) * 10 };
            px.handle_frame_at(&cfg, t0 + std::time::Duration::from_millis(ms))
                .unwrap();
            let events = wire_events(&sinks.pointer);
            for (slot, id) in tracking_changes(&events[mark..]) {
                if slot == 0 {
                    assert!(id >= 0, "resting finger ended in frame {i}: {events:?}");
                    starts += 1;
                    assert_eq!(starts, 1, "resting finger restarted in frame {i}");
                }
            }
            // The resting position must still be delivered to its original
            // slot, even when it is last in the report or others lift.
            assert!(events[mark..].windows(2).any(|w| {
                w[0] == (EV_ABS, ABS_MT_SLOT, 0)
                    && (w[1] == (EV_ABS, ABS_MT_POSITION_X, 100) || w[1].1 == ABS_MT_TRACKING_ID)
            }));
        }
        assert_eq!(starts, 1);
        assert_eq!(key_values(&sinks.pointer, BTN_TOUCH), vec![1; n]);
        assert_eq!(key_values(&sinks.pointer, BTN_TOOL_FINGER).last(), Some(&1));
        assert!(!sinks.pointer.has(EV_KEY, BTN_LEFT, 1));
        assert!(haptic.calls.lock().unwrap().is_empty());
    }
}

#[test]
fn reordered_contacts_keep_their_slots_and_positions() {
    let sinks = TestSinks::new();
    let haptic = Arc::new(FakeHaptic::new());
    let a = finger(7, 9, 100, 200);
    let b = finger(2, 9, 300, 400);
    let frames = vec![
        Ok(Some(frame(vec![a, b], 0))),
        Ok(Some(frame(vec![b, a], 0))),
        // Removing the first report contact must not compact the survivor.
        Ok(Some(frame(vec![b], 0))),
    ];
    let mut px = drive_ready_proxy(&sinks, &haptic, frames);
    let cfg = Config::default();
    px.handle_frame(&cfg).unwrap();
    let mark = wire_events(&sinks.pointer).len();
    px.handle_frame(&cfg).unwrap();
    let events = wire_events(&sinks.pointer);
    let update = &events[mark..];
    assert!(tracking_changes(update).is_empty());
    assert!(update
        .windows(2)
        .any(|w| { w == [(EV_ABS, ABS_MT_SLOT, 1), (EV_ABS, ABS_MT_POSITION_X, 300)] }));
    assert!(update
        .windows(2)
        .any(|w| { w == [(EV_ABS, ABS_MT_SLOT, 0), (EV_ABS, ABS_MT_POSITION_X, 100)] }));
    let mark = events.len();
    px.handle_frame(&cfg).unwrap();
    assert_eq!(
        tracking_changes(&wire_events(&sinks.pointer)[mark..]),
        vec![(0, -1)]
    );
}

#[test]
fn reused_hardware_id_starts_a_fresh_tracking_lifetime() {
    for disappearance in [false, true] {
        let sinks = TestSinks::new();
        let haptic = Arc::new(FakeHaptic::new());
        let held = finger(7, 9, 100, 200);
        let mut up = held;
        up.contact = false;
        up.pressure = 0;
        let frames = vec![
            Ok(Some(frame(vec![held], 0))),
            Ok(Some(frame(
                if disappearance { vec![] } else { vec![up] },
                0,
            ))),
            Ok(Some(frame(vec![held], 0))),
        ];
        let mut px = drive_ready_proxy(&sinks, &haptic, frames);
        for _ in 0..3 {
            px.handle_frame(&Config::default()).unwrap();
        }
        let changes = tracking_changes(&wire_events(&sinks.pointer));
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[1], (changes[0].0, -1));
        assert!(changes[0].1 >= 0 && changes[2].1 >= 0);
        assert_ne!(changes[0].1, changes[2].1);
    }
}

#[test]
fn tracking_id_wrap_skips_ids_of_contacts_still_held() {
    let sinks = TestSinks::new();
    let haptic = Arc::new(FakeHaptic::new());
    let a = finger(7, 9, 100, 200);
    let b = finger(6, 9, 300, 400);
    let c = finger(5, 9, 500, 600);
    let frames = vec![
        Ok(Some(frame(vec![a], 0))),
        Ok(Some(frame(vec![a, b], 0))),
        Ok(Some(frame(vec![a, b, c], 0))),
    ];
    let mut px = drive_ready_proxy(&sinks, &haptic, frames);
    px.next_tracking_id = u16::MAX;
    px.handle_frame(&Config::default()).unwrap();
    px.handle_frame(&Config::default()).unwrap();
    // Simulate the counter wrapping again while both contacts remain down.
    px.next_tracking_id = u16::MAX;
    px.handle_frame(&Config::default()).unwrap();
    assert_eq!(
        tracking_changes(&wire_events(&sinks.pointer)),
        vec![(0, 65535), (1, 0), (2, 1)]
    );
}

#[test]
fn contact_replacement_uses_an_already_free_slot_when_available() {
    let sinks = TestSinks::new();
    let haptic = Arc::new(FakeHaptic::new());
    let frames = vec![
        Ok(Some(frame(vec![finger(7, 9, 100, 200)], 0))),
        Ok(Some(frame(vec![finger(8, 9, 300, 400)], 0))),
    ];
    let mut px = drive_ready_proxy(&sinks, &haptic, frames);
    px.handle_frame(&Config::default()).unwrap();
    let mark = wire_events(&sinks.pointer).len();
    px.handle_frame(&Config::default()).unwrap();
    let changes = tracking_changes(&wire_events(&sinks.pointer)[mark..]);
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0], (0, -1));
    assert!(changes[1].0 != changes[0].0 && changes[1].1 >= 0);
}

#[test]
fn spoof_count_changes_terminate_removed_virtual_contacts() {
    let sinks = TestSinks::new();
    let haptic = Arc::new(FakeHaptic::new());
    let frames = (0..3)
        .map(|_| Ok(Some(frame(vec![finger(7, 9, 100, 200)], 0))))
        .collect();
    let mut px = drive_ready_proxy(&sinks, &haptic, frames);
    let mut cfg = Config::default();
    cfg.spoof.slide.insert(1, 3);
    px.handle_frame(&cfg).unwrap();
    cfg.spoof.slide.insert(1, 2);
    let mark = wire_events(&sinks.pointer).len();
    px.handle_frame(&cfg).unwrap();
    assert_eq!(
        tracking_changes(&wire_events(&sinks.pointer)[mark..]),
        vec![(2, -1)]
    );
    cfg.spoof.slide.clear();
    let mark = wire_events(&sinks.pointer).len();
    px.handle_frame(&cfg).unwrap();
    let changes = tracking_changes(&wire_events(&sinks.pointer)[mark..]);
    assert!(changes.contains(&(0, -1)) && changes.contains(&(1, -1)));
    assert_eq!(changes.iter().filter(|(_, id)| *id >= 0).count(), 1);
    assert_eq!(key_values(&sinks.pointer, BTN_TOOL_FINGER).last(), Some(&1));
}
