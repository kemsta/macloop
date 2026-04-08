#[derive(Debug, Clone, Copy)]
pub(crate) struct AudioBufferRef<'a> {
    pub samples: &'a [f32],
    pub channels: usize,
}

#[cfg(target_os = "macos")]
fn shareable_content_from_retained_ptr(
    ptr: *const std::ffi::c_void,
) -> screencapturekit::shareable_content::SCShareableContent {
    use screencapturekit::shareable_content::SCShareableContent;

    // SAFETY:
    // - `sc_shareable_content_get_sync` returns the same retained opaque pointer type used by the
    //   `screencapturekit` crate for `SCShareableContent`.
    // - `SCShareableContent` is `repr(transparent)` over that pointer in the current dependency.
    // - We isolate the dependency-layout assumption here so the rest of the discovery path stays
    //   safe and the invariant is documented in one place.
    unsafe { std::mem::transmute::<*const std::ffi::c_void, SCShareableContent>(ptr) }
}

#[cfg(target_os = "macos")]
pub(crate) fn get_shareable_content_with_timeout(
) -> Result<screencapturekit::shareable_content::SCShareableContent, String> {
    use screencapturekit::ffi;
    use std::ffi::CStr;
    use std::os::raw::c_char;

    let mut error_buffer = vec![0 as c_char; 1024];
    let ptr = unsafe {
        ffi::sc_shareable_content_get_sync(
            false,
            false,
            error_buffer.as_mut_ptr(),
            error_buffer.len() as isize,
        )
    };

    if ptr.is_null() {
        let error = unsafe { CStr::from_ptr(error_buffer.as_ptr()) }
            .to_str()
            .ok()
            .map(str::trim)
            .filter(|msg| !msg.is_empty())
            .unwrap_or("failed to retrieve shareable content")
            .to_string();
        return Err(error);
    }

    Ok(shareable_content_from_retained_ptr(ptr))
}

pub(crate) fn normalize_audio_buffers_into_scratch(
    buffers: &[AudioBufferRef<'_>],
    target_channels: usize,
    scratch: &mut Vec<f32>,
) {
    scratch.clear();

    if target_channels == 0 || buffers.is_empty() {
        return;
    }

    let first = buffers[0];
    if buffers.len() == 1 {
        let input_channels = first.channels.max(1);
        let frames = first.samples.len() / input_channels;
        scratch.reserve(frames * target_channels);

        for frame in first.samples.chunks_exact(input_channels) {
            if input_channels == 1 {
                let sample = frame[0];
                for _ in 0..target_channels {
                    scratch.push(sample);
                }
            } else {
                for sample in frame.iter().take(target_channels) {
                    scratch.push(*sample);
                }
            }
        }
        return;
    }

    let mut frames = usize::MAX;
    for buffer in buffers {
        let channels = buffer.channels.max(1);
        frames = frames.min(buffer.samples.len() / channels);
    }

    if frames == usize::MAX || frames == 0 {
        return;
    }

    scratch.reserve(frames * target_channels);

    for frame_index in 0..frames {
        let mut emitted = 0_usize;
        let mut fallback_sample = 0.0_f32;
        let mut have_fallback = false;

        for buffer in buffers {
            let channels = buffer.channels.max(1);
            let base = frame_index * channels;

            for channel_index in 0..channels {
                let sample = buffer.samples[base + channel_index];
                if !have_fallback {
                    fallback_sample = sample;
                    have_fallback = true;
                }
                if emitted < target_channels {
                    scratch.push(sample);
                    emitted += 1;
                }
            }

            if emitted >= target_channels {
                break;
            }
        }

        if have_fallback {
            while emitted < target_channels {
                scratch.push(fallback_sample);
                emitted += 1;
            }
        }
    }
}

pub(crate) fn select_item_by_id<'a, T, E>(
    items: &'a [T],
    selected_id: Option<u32>,
    id_of: impl Fn(&T) -> u32,
    no_items: impl FnOnce() -> E,
    not_found: impl FnOnce(u32) -> E,
) -> Result<&'a T, E> {
    match selected_id {
        Some(selected_id) => items
            .iter()
            .find(|item| id_of(item) == selected_id)
            .ok_or_else(|| not_found(selected_id)),
        None => items.first().ok_or_else(no_items),
    }
}

pub(crate) fn select_items_by_ids<'a, T, E>(
    items: &'a [T],
    selected_ids: &[u32],
    id_of: impl Fn(&T) -> u32,
    no_selection: impl FnOnce() -> E,
    no_items: impl FnOnce() -> E,
    not_found: impl FnOnce(Vec<u32>) -> E,
) -> Result<Vec<&'a T>, E> {
    if selected_ids.is_empty() {
        return Err(no_selection());
    }

    if items.is_empty() {
        return Err(no_items());
    }

    let mut selected = Vec::with_capacity(selected_ids.len());
    let mut missing_ids = Vec::new();

    for selected_id in selected_ids {
        match items.iter().find(|item| id_of(item) == *selected_id) {
            Some(item) => selected.push(item),
            None => missing_ids.push(*selected_id),
        }
    }

    if !missing_ids.is_empty() {
        return Err(not_found(missing_ids));
    }

    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Item {
        id: u32,
    }

    #[test]
    fn select_item_by_id_uses_first_item_when_not_specified() {
        let items = [Item { id: 10 }, Item { id: 20 }];
        let selected =
            select_item_by_id(&items, None, |item| item.id, || "no items", |_| "not found")
                .expect("select default item");

        assert_eq!(selected.id, 10);
    }

    #[test]
    fn select_item_by_id_on_empty_items_returns_no_items() {
        let err = select_item_by_id::<Item, String>(
            &[],
            None,
            |item| item.id,
            || "no items".to_string(),
            |id| format!("missing {id}"),
        )
        .expect_err("empty items");

        assert_eq!(err, "no items");
    }

    #[test]
    fn select_item_by_id_with_explicit_id_returns_item() {
        let items = [Item { id: 10 }, Item { id: 20 }];
        let selected = select_item_by_id(
            &items,
            Some(20),
            |item| item.id,
            || "no items".to_string(),
            |id| format!("missing {id}"),
        )
        .expect("select explicit item");

        assert_eq!(selected.id, 20);
    }

    #[test]
    fn select_item_by_id_reports_missing_item() {
        let items = [Item { id: 10 }, Item { id: 20 }];
        let err = select_item_by_id(
            &items,
            Some(30),
            |item| item.id,
            || "no items".to_string(),
            |id| format!("missing {id}"),
        )
        .expect_err("missing item");

        assert_eq!(err, "missing 30");
    }

    #[test]
    fn select_items_by_ids_preserves_requested_order() {
        let items = [Item { id: 10 }, Item { id: 20 }, Item { id: 30 }];
        let selected = select_items_by_ids(
            &items,
            &[30, 10],
            |item| item.id,
            || "no selection".to_string(),
            || "no items".to_string(),
            |ids| format!("missing {ids:?}"),
        )
        .expect("select ordered items");

        assert_eq!(
            selected.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![30, 10]
        );
    }

    #[test]
    fn select_items_by_ids_requires_non_empty_selection() {
        let items = [Item { id: 10 }];
        let err = select_items_by_ids(
            &items,
            &[],
            |item| item.id,
            || "no selection".to_string(),
            || "no items".to_string(),
            |ids| format!("missing {ids:?}"),
        )
        .expect_err("empty selection");

        assert_eq!(err, "no selection");
    }

    #[test]
    fn select_items_by_ids_empty_selection_takes_priority_over_no_items() {
        let err = select_items_by_ids::<Item, String>(
            &[],
            &[],
            |item| item.id,
            || "no selection".to_string(),
            || "no items".to_string(),
            |ids| format!("missing {ids:?}"),
        )
        .expect_err("empty selection should win");

        assert_eq!(err, "no selection");
    }

    #[test]
    fn select_items_by_ids_preserves_duplicate_ids() {
        let items = [Item { id: 10 }, Item { id: 20 }];
        let selected = select_items_by_ids(
            &items,
            &[10, 10, 20],
            |item| item.id,
            || "no selection".to_string(),
            || "no items".to_string(),
            |ids| format!("missing {ids:?}"),
        )
        .expect("duplicate ids");

        assert_eq!(
            selected.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![10, 10, 20]
        );
    }

    #[test]
    fn select_items_by_ids_reports_all_missing_ids() {
        let items = [Item { id: 10 }];
        let err = select_items_by_ids(
            &items,
            &[10, 20, 30],
            |item| item.id,
            || "no selection".to_string(),
            || "no items".to_string(),
            |ids| format!("missing {ids:?}"),
        )
        .expect_err("missing ids");

        assert_eq!(err, "missing [20, 30]");
    }

    #[test]
    fn normalize_single_mono_buffer_duplicates_to_stereo() {
        let mut scratch = Vec::new();
        normalize_audio_buffers_into_scratch(
            &[AudioBufferRef {
                samples: &[1.0, 2.0, 3.0],
                channels: 1,
            }],
            2,
            &mut scratch,
        );

        assert_eq!(scratch, vec![1.0, 1.0, 2.0, 2.0, 3.0, 3.0]);
    }

    #[test]
    fn normalize_single_interleaved_buffer_preserves_stereo() {
        let mut scratch = Vec::new();
        normalize_audio_buffers_into_scratch(
            &[AudioBufferRef {
                samples: &[1.0, 10.0, 2.0, 20.0],
                channels: 2,
            }],
            2,
            &mut scratch,
        );

        assert_eq!(scratch, vec![1.0, 10.0, 2.0, 20.0]);
    }

    #[test]
    fn normalize_planar_mono_buffers_interleaves_stereo() {
        let mut scratch = Vec::new();
        normalize_audio_buffers_into_scratch(
            &[
                AudioBufferRef {
                    samples: &[1.0, 2.0],
                    channels: 1,
                },
                AudioBufferRef {
                    samples: &[10.0, 20.0],
                    channels: 1,
                },
            ],
            2,
            &mut scratch,
        );

        assert_eq!(scratch, vec![1.0, 10.0, 2.0, 20.0]);
    }

    #[test]
    fn normalize_multi_buffer_truncates_to_target_channels() {
        let mut scratch = Vec::new();
        normalize_audio_buffers_into_scratch(
            &[
                AudioBufferRef {
                    samples: &[1.0, 2.0],
                    channels: 1,
                },
                AudioBufferRef {
                    samples: &[10.0, 20.0],
                    channels: 1,
                },
                AudioBufferRef {
                    samples: &[100.0, 200.0],
                    channels: 1,
                },
            ],
            2,
            &mut scratch,
        );

        assert_eq!(scratch, vec![1.0, 10.0, 2.0, 20.0]);
    }

    #[test]
    fn normalize_single_buffer_truncates_extra_channels() {
        let mut scratch = Vec::new();
        normalize_audio_buffers_into_scratch(
            &[AudioBufferRef {
                samples: &[1.0, 10.0, 100.0, 2.0, 20.0, 200.0],
                channels: 3,
            }],
            2,
            &mut scratch,
        );

        assert_eq!(scratch, vec![1.0, 10.0, 2.0, 20.0]);
    }

    #[test]
    fn normalize_multi_buffer_fills_missing_target_channels_from_fallback() {
        let mut scratch = Vec::new();
        normalize_audio_buffers_into_scratch(
            &[
                AudioBufferRef {
                    samples: &[1.0, 2.0],
                    channels: 1,
                },
                AudioBufferRef {
                    samples: &[10.0, 20.0],
                    channels: 1,
                },
            ],
            3,
            &mut scratch,
        );

        assert_eq!(scratch, vec![1.0, 10.0, 1.0, 2.0, 20.0, 2.0]);
    }

    #[test]
    fn normalize_empty_buffers_produces_empty_scratch() {
        let mut scratch = vec![99.0, 100.0];
        normalize_audio_buffers_into_scratch(&[], 2, &mut scratch);
        assert!(scratch.is_empty());
    }

    #[test]
    fn normalize_zero_channel_buffer_treated_as_mono() {
        let mut scratch = Vec::new();
        normalize_audio_buffers_into_scratch(
            &[AudioBufferRef {
                samples: &[1.0, 2.0],
                channels: 0,
            }],
            2,
            &mut scratch,
        );
        assert_eq!(scratch, vec![1.0, 1.0, 2.0, 2.0]);
    }

    #[test]
    fn normalize_zero_target_channels_produces_empty_scratch() {
        let mut scratch = vec![99.0, 100.0];
        normalize_audio_buffers_into_scratch(
            &[AudioBufferRef {
                samples: &[1.0, 2.0],
                channels: 1,
            }],
            0,
            &mut scratch,
        );
        assert!(scratch.is_empty());
    }

    #[test]
    fn select_items_by_ids_on_empty_items_returns_no_items_error() {
        let err = select_items_by_ids::<Item, String>(
            &[],
            &[10],
            |item| item.id,
            || "no selection".to_string(),
            || "no items".to_string(),
            |ids| format!("missing {ids:?}"),
        )
        .expect_err("empty items");

        assert_eq!(err, "no items");
    }

    #[test]
    fn normalize_uses_shortest_buffer_length() {
        let mut scratch = Vec::new();
        normalize_audio_buffers_into_scratch(
            &[
                AudioBufferRef {
                    samples: &[1.0, 2.0, 3.0],
                    channels: 1,
                },
                AudioBufferRef {
                    samples: &[10.0, 20.0],
                    channels: 1,
                },
            ],
            2,
            &mut scratch,
        );

        assert_eq!(scratch, vec![1.0, 10.0, 2.0, 20.0]);
    }
}
