//! Candidate cards of the release review page: one saved candidate with its
//! scores, notes, the files side by side with the provider tracks they map to,
//! and the accept form.

// Rust guideline compliant 2026-08-14

use super::{View, escape_html, hidden_fields, is_web_url};
use crate::layout::{format_duration, plural, track_number};
use crate::matcher::{self, Candidate, Evaluation, LocalRelease, RemoteTrack};
use crate::providers::{ProviderKind, ReleaseId};
use crate::review as saved;

/// One candidate as a card. Only a `current` card has a track table and an
/// accept section: an out-of-date mapping indexes files that may have moved.
pub(super) fn render_card(
    view: &View<'_>,
    provider: ProviderKind,
    index: usize,
    candidate: &Candidate,
    evaluation: &Evaluation,
    current: bool,
) -> String {
    let release = &candidate.release;
    let local = &view.review.release;
    let id = escape_html(&release.id.storage_id());
    let id = if is_web_url(&release.url) {
        format!(
            "<a href=\"{}\" rel=\"noopener noreferrer\">{id}</a>",
            escape_html(&release.url)
        )
    } else {
        id
    };
    let verdict = if evaluation.auto_accept {
        "auto-acceptable"
    } else {
        "review"
    };
    let details = saved::edition_details(release);
    let details = if details.is_empty() {
        String::new()
    } else {
        format!(
            "<p class=\"muted\">{}</p>",
            escape_html(&details.join(" · "))
        )
    };
    let mut notes: Vec<String> = Vec::new();
    notes.extend(
        release
            .structure_issues
            .iter()
            .map(|note| format!("structure: {note}")),
    );
    if !release.lookup_aliases.is_empty() {
        let aliases: Vec<String> = release
            .lookup_aliases
            .iter()
            .map(ReleaseId::storage_id)
            .collect();
        notes.push(format!("aliases: {}", aliases.join(", ")));
    }
    notes.extend(
        evaluation
            .evidence
            .iter()
            .map(|note| format!("evidence: {note}")),
    );
    if !current {
        // A current card shows its blockers above the Accept button instead.
        notes.extend(
            evaluation
                .blockers
                .iter()
                .map(|note| format!("blocker: {note}")),
        );
    }
    notes.extend(
        evaluation
            .warnings
            .iter()
            .map(|note| format!("warning: {note}")),
    );
    notes.extend(
        candidate
            .mapping
            .notes
            .iter()
            .map(|note| format!("mapping: {note}")),
    );
    let notes: String = notes
        .iter()
        .map(|note| format!("<li>{}</li>", escape_html(note)))
        .collect();
    let disc = candidate
        .mapping
        .medium
        .map(|medium| {
            format!(
                "<p>The files map onto {}.</p>",
                escape_html(&matcher::disc_label(release, medium))
            )
        })
        .unwrap_or_default();
    let (tracks, action) = if current {
        (
            render_mapping(local, candidate),
            render_accept(view, provider, candidate, evaluation),
        )
    } else {
        (String::new(), String::new())
    };
    format!(
        "<section class=\"card\"><h3>[{}] {provider} {id} — score {:.3} \
         <span class=\"muted\">{verdict}</span></h3><p>{} — {}</p>{details}\
         <p class=\"muted\">score: {}</p><p class=\"muted\">mapping: {}/{} files mapped</p>\
         <ul>{notes}</ul>{disc}{tracks}{action}</section>",
        index + 1,
        evaluation.match_scope.score(&candidate.score),
        escape_html(&release.artist_display()),
        escape_html(&release.title),
        escape_html(&saved::score_parts(candidate, evaluation).join("  ")),
        candidate.mapping.assigned_count(),
        local.tracks.len(),
    )
}

/// The most provider tracks no file maps to that a track table lists.
///
/// A release of a few files can map onto a box set of thousands of tracks;
/// listing them all would make the page megabytes long. The rest are counted.
const MAX_UNMAPPED_ROWS: usize = 25;

/// The files side by side with the provider tracks they map to, then the
/// provider's playable tracks that no file maps to (on the mapped disc only,
/// at most [`MAX_UNMAPPED_ROWS`] of them).
fn render_mapping(local: &LocalRelease, candidate: &Candidate) -> String {
    let remote = &candidate.release.tracklist;
    let media = candidate.release.track_media();
    // A position such as `3` only means something with its disc beside it.
    // Counted the way `disc_label` counts, so a CD plus a video DVD, whose
    // tracks are not audio, keeps its plain positions.
    let several_media = matcher::remote_media(&candidate.release).is_some();
    let cells = |remote_index: usize| {
        remote_cells(
            &remote[remote_index],
            media[remote_index].filter(|_| several_media),
        )
    };
    let mut shown = vec![false; remote.len()];
    let mut rows = String::new();
    for (index, track) in local.tracks.iter().enumerate() {
        let paired = candidate
            .mapping
            .pairs
            .get(index)
            .copied()
            .flatten()
            .filter(|&remote_index| remote_index < remote.len());
        if let Some(remote_index) = paired {
            shown[remote_index] = true;
        }
        rows.push_str(&format!(
            "<tr><td class=\"num\">{}</td><td>{}</td><td class=\"num\">{}</td>{}</tr>",
            track_number(track.disc, track.track_number),
            escape_html(track.title.as_deref().unwrap_or("")),
            format_duration(track.duration_secs),
            paired.map_or_else(
                || "<td>—</td><td class=\"muted\">not mapped</td><td></td>".to_string(),
                cells
            ),
        ));
    }
    let mut unmapped = 0_usize;
    for (remote_index, track) in remote.iter().enumerate() {
        let on_disc = candidate
            .mapping
            .medium
            .is_none_or(|medium| media[remote_index] == Some(medium));
        if shown[remote_index] || !track.is_playable() || !on_disc {
            continue;
        }
        unmapped += 1;
        if unmapped <= MAX_UNMAPPED_ROWS {
            rows.push_str(&format!(
                "<tr><td></td><td class=\"muted\">not in the files</td><td></td>{}</tr>",
                cells(remote_index)
            ));
        }
    }
    if let Some(more) = unmapped
        .checked_sub(MAX_UNMAPPED_ROWS)
        .filter(|&more| more > 0)
    {
        rows.push_str(&format!(
            "<tr><td></td><td class=\"muted\" colspan=\"5\">… and {more} more provider \
             {} not in the files</td></tr>",
            if more == 1 { "track" } else { "tracks" }
        ));
    }
    format!(
        "<div class=\"table\"><table><thead><tr><th class=\"num\">#</th><th>File title</th>\
         <th class=\"num\">File time</th><th>Pos</th><th>{label} title</th>\
         <th class=\"num\">{label} time</th></tr></thead><tbody>{rows}</tbody></table></div>",
        label = candidate.release.provider().label()
    )
}

/// The provider side of a track table row.
///
/// `medium` is the disc to show before the position, for a release of several
/// discs; a position that already names it (`2-3`) is left as it is.
fn remote_cells(track: &RemoteTrack, medium: Option<u32>) -> String {
    let position = track.position.as_deref().unwrap_or("");
    let position = match medium {
        Some(medium) if !position.is_empty() && !position.starts_with(&format!("{medium}-")) => {
            format!("{medium}-{position}")
        }
        _ => position.to_string(),
    };
    format!(
        "<td>{}</td><td>{}</td><td class=\"num\">{}</td>",
        escape_html(&position),
        escape_html(&track.title),
        format_duration(track.duration_secs),
    )
}

/// The text after `Cannot accept:` for a mapping that cannot be applied.
pub(super) fn cannot_apply_text(notes: &[String]) -> String {
    if notes.is_empty() {
        "the track mapping is invalid, incomplete, or structurally unsupported".to_string()
    } else {
        notes.join("; ")
    }
}

/// The blockers, then the accept button, the command that does the same,
/// or why the candidate cannot be accepted.
fn render_accept(
    view: &View<'_>,
    provider: ProviderKind,
    candidate: &Candidate,
    evaluation: &Evaluation,
) -> String {
    let local = &view.review.release;
    let blockers: String = evaluation
        .blockers
        .iter()
        .map(|blocker| format!("<li>{}</li>", escape_html(blocker)))
        .collect();
    if !evaluation.can_apply(local, candidate) {
        let blockers = if blockers.is_empty() {
            String::new()
        } else {
            format!("<ul class=\"blocker\">{blockers}</ul>")
        };
        return format!(
            "{blockers}<p class=\"blocker\">Cannot accept: {}</p>",
            escape_html(&cannot_apply_text(&candidate.mapping.notes))
        );
    }
    let blockers = if blockers.is_empty() {
        String::new()
    } else {
        format!("<p>Accepting overrides these checks:</p><ul class=\"blocker\">{blockers}</ul>")
    };
    let Some(token) = view.token else {
        return format!(
            "{blockers}<p class=\"muted\">This server listens on a non-loopback address, so it \
             changes nothing. Run <code>radish match --review --local-release {}</code> with the \
             same <code>--db</code> as this server.</p>",
            local.id
        );
    };
    let label = match evaluation.blockers.len() {
        0 => "Accept".to_string(),
        count => format!(
            "Accept, overriding {}",
            plural(count as u64, "check", "checks")
        ),
    };
    let id = escape_html(&candidate.release.id.storage_id());
    format!(
        "{blockers}<form method=\"post\" action=\"/review\">{}\
         <input type=\"hidden\" name=\"action\" value=\"accept\">\
         <input type=\"hidden\" name=\"provider\" value=\"{provider}\">\
         <input type=\"hidden\" name=\"candidate\" value=\"{id}\">\
         <button aria-label=\"{label}: {provider} {id}\">{label}</button></form>",
        hidden_fields(view, token),
    )
}

#[cfg(test)]
mod tests {
    use super::super::render_release;
    use super::super::tests::{FILES, exact, group, review_of, view};
    use super::*;
    use crate::matcher::RemoteRelease;
    use crate::review::GroupState;
    use crate::test_support::{FIXTURE_MBID, OTHER_MBID, mbid_release, remote_release};

    fn longer() -> RemoteRelease {
        remote_release(
            mbid_release(OTHER_MBID),
            "Blue",
            &["<script>x</script>", "Two", "Bonus"],
        )
    }

    #[test]
    fn a_candidate_card_escapes_everything_and_maps_track_by_track() {
        let review = review_of(vec![group(
            GroupState::Current,
            vec![
                (exact(), vec!["<b>close</b>".into()]),
                (longer(), Vec::new()),
            ],
        )]);
        let html = render_release(&view(&review, Some("tok")));
        for raw in ["<script", "<i>", "<b>", "<u>"] {
            assert!(!html.contains(raw), "{raw} in {html}");
        }
        assert!(html.contains("&lt;script&gt;x&lt;/script&gt;"), "{html}");
        assert!(html.contains("Accepting overrides these checks:"), "{html}");
        assert!(
            html.contains(">Accept, overriding 1 check</button>"),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                "<button aria-label=\"Accept, overriding 1 check: musicbrainz {FIXTURE_MBID}\">"
            )),
            "{html}"
        );
        assert!(html.contains("<th class=\"num\">File time</th>"), "{html}");
        assert!(
            html.contains("<th class=\"num\">MusicBrainz time</th>"),
            "{html}"
        );
        assert!(
            html.contains(&format!("name=\"candidate\" value=\"{FIXTURE_MBID}\"")),
            "{html}"
        );
        assert!(
            !html.contains(&format!("name=\"candidate\" value=\"{OTHER_MBID}\"")),
            "{html}"
        );
        assert!(html.contains("Cannot accept:"), "{html}");
        assert!(html.contains("not in the files"), "{html}");
        assert!(html.contains("name=\"revision\" value=\"rev\""), "{html}");
        assert!(html.contains("name=\"match\" value=\"\""), "{html}");
        assert!(html.contains("name=\"token\" value=\"tok\""), "{html}");
        assert!(
            html.contains("Queued: &lt;b&gt;close&lt;/b&gt; alternative"),
            "{html}"
        );
        assert!(
            html.contains("<a href=\"/reviews\">Back to the queue</a>"),
            "{html}"
        );
        assert!(
            html.contains("<option value=\"musicbrainz\">MusicBrainz</option>"),
            "{html}"
        );
    }

    #[test]
    fn a_server_without_writes_shows_commands_instead_of_forms() {
        let review = review_of(vec![group(
            GroupState::Current,
            vec![(exact(), Vec::new())],
        )]);
        let html = render_release(&view(&review, None));
        assert!(!html.contains("<form"), "{html}");
        assert!(
            html.contains("radish match --review --local-release 12"),
            "{html}"
        );
        assert!(html.contains("--musicbrainz-release MBID"), "{html}");
    }

    /// `remote_release` of `FILES` followed by `extra` more tracks.
    fn remote_with_extra(extra: usize) -> RemoteRelease {
        let more: Vec<String> = (0..extra).map(|index| format!("Extra {index}")).collect();
        let mut titles: Vec<&str> = FILES.to_vec();
        titles.extend(more.iter().map(String::as_str));
        remote_release(mbid_release(FIXTURE_MBID), "<i>Blue</i>", &titles)
    }

    #[test]
    fn unmapped_provider_tracks_are_listed_up_to_a_limit_and_then_counted() {
        let review = review_of(vec![group(
            GroupState::Current,
            vec![(remote_with_extra(38), Vec::new())],
        )]);
        let html = render_release(&view(&review, Some("tok")));
        assert_eq!(
            html.matches("<td class=\"muted\">not in the files</td>")
                .count(),
            MAX_UNMAPPED_ROWS,
            "{html}"
        );
        assert!(
            html.contains("… and 13 more provider tracks not in the files"),
            "{html}"
        );
        assert!(html.contains("Extra 0"), "{html}");
        assert!(!html.contains("Extra 37"), "{html}");

        // Exactly at the limit there is nothing to count.
        let review = review_of(vec![group(
            GroupState::Current,
            vec![(remote_with_extra(MAX_UNMAPPED_ROWS), Vec::new())],
        )]);
        let html = render_release(&view(&review, Some("tok")));
        assert!(!html.contains("more provider"), "{html}");
    }

    #[test]
    fn unmapped_provider_tracks_come_after_the_mapped_rows() {
        let review = review_of(vec![group(
            GroupState::Current,
            vec![(longer(), Vec::new())],
        )]);
        let html = render_release(&view(&review, Some("tok")));
        let unmapped = html.find("not in the files").expect("an unmapped row");
        let last_mapped = html.rfind("<td>Two</td>").expect("a mapped row");
        assert!(last_mapped < unmapped, "{html}");
        assert!(html.find("<td>Bonus</td>").unwrap() > unmapped, "{html}");
    }

    #[test]
    fn a_mapping_shorter_than_the_files_still_lists_every_file() {
        let mut review = review_of(vec![group(
            GroupState::Current,
            vec![(exact(), Vec::new())],
        )]);
        review.groups[0].candidates[0].0.mapping.pairs.truncate(1);
        let html = render_release(&view(&review, Some("tok")));
        assert!(
            html.contains("<td>Two</td><td class=\"num\">3:00</td><td>—</td>"),
            "{html}"
        );
        assert!(html.contains("not mapped"), "{html}");
    }

    #[test]
    fn positions_name_the_disc_only_on_a_release_of_several_discs() {
        let mut remote = remote_release(
            mbid_release(FIXTURE_MBID),
            "<i>Blue</i>",
            &["A", "B", FILES[0], FILES[1]],
        );
        for (index, track) in remote.tracklist.iter_mut().enumerate() {
            track.medium_position = Some(index as u32 / 2 + 1);
            track.track_position = Some(index as u32 % 2 + 1);
            track.position = Some((index % 2 + 1).to_string());
        }
        let review = review_of(vec![group(GroupState::Current, vec![(remote, Vec::new())])]);
        let html = render_release(&view(&review, Some("tok")));
        assert!(
            html.contains("<td>2-1</td><td>&lt;script&gt;x&lt;/script&gt;</td>"),
            "{html}"
        );
        assert!(html.contains("<td>2-2</td><td>Two</td>"), "{html}");

        // One disc keeps its plain positions.
        let review = review_of(vec![group(
            GroupState::Current,
            vec![(exact(), Vec::new())],
        )]);
        let html = render_release(&view(&review, Some("tok")));
        assert!(
            html.contains("<td>1</td><td>&lt;script&gt;x&lt;/script&gt;</td>"),
            "{html}"
        );
        assert!(!html.contains("<td>1-1</td>"), "{html}");
    }

    #[test]
    fn an_out_of_date_card_lists_its_blockers_as_notes_without_a_button() {
        let review = review_of(vec![group(
            GroupState::OutOfDate,
            vec![(exact(), vec!["<b>close</b>".into()])],
        )]);
        let html = render_release(&view(&review, Some("tok")));
        assert!(
            html.contains("<li>blocker: &lt;b&gt;close&lt;/b&gt;</li>"),
            "{html}"
        );
        assert!(!html.contains("<button aria-label"), "{html}");

        // A current card keeps them above its button instead.
        let review = review_of(vec![group(
            GroupState::Current,
            vec![(exact(), vec!["<b>close</b>".into()])],
        )]);
        let html = render_release(&view(&review, Some("tok")));
        assert!(!html.contains("blocker: "), "{html}");
    }

    #[test]
    fn a_card_without_blockers_says_plain_accept_and_links_only_web_urls() {
        let mut remote = exact();
        remote.url = "javascript:alert(1)".into();
        let review = review_of(vec![group(GroupState::Current, vec![(remote, Vec::new())])]);
        let html = render_release(&view(&review, Some("tok")));
        assert!(html.contains(">Accept</button>"), "{html}");
        assert!(
            html.contains(&format!(
                "aria-label=\"Accept: musicbrainz {FIXTURE_MBID}\""
            )),
            "{html}"
        );
        assert!(!html.contains("javascript"), "{html}");
        assert!(html.contains(FIXTURE_MBID), "{html}");
    }

    #[test]
    fn positions_that_already_name_the_disc_are_not_prefixed_again() {
        let mut remote = remote_release(
            mbid_release(FIXTURE_MBID),
            "<i>Blue</i>",
            &["A", "B", FILES[0], FILES[1]],
        );
        for (index, track) in remote.tracklist.iter_mut().enumerate() {
            let (medium, position) = (index as u32 / 2 + 1, index as u32 % 2 + 1);
            track.medium_position = Some(medium);
            track.track_position = Some(position);
            track.position = Some(format!("{medium}-{position}"));
        }
        let review = review_of(vec![group(GroupState::Current, vec![(remote, Vec::new())])]);
        let html = render_release(&view(&review, Some("tok")));
        assert!(html.contains("<td>2-1</td>"), "{html}");
        assert!(!html.contains("2-2-"), "{html}");
        assert!(!html.contains("1-1-"), "{html}");
    }

    #[test]
    fn a_disc_without_a_named_medium_keeps_plain_positions() {
        // Two discs and an audio track that names no disc.
        let mut remote = remote_with_extra(1);
        for (index, medium) in [(0, 1), (1, 2)] {
            remote.tracklist[index].medium_position = Some(medium);
            remote.tracklist[index].track_position = Some(1);
        }
        let review = review_of(vec![group(GroupState::Current, vec![(remote, Vec::new())])]);
        let html = render_release(&view(&review, Some("tok")));
        assert!(
            !html.contains("<td>1-1</td>") && !html.contains("<td>2-2</td>"),
            "{html}"
        );
        assert!(!html.contains("<td>2-1</td>"), "{html}");
        assert!(html.contains("<td>2</td><td>Two</td>"), "{html}");
    }

    #[test]
    fn a_pair_pointing_past_the_provider_tracks_shows_the_file_as_not_mapped() {
        let mut review = review_of(vec![group(
            GroupState::Current,
            vec![(exact(), Vec::new())],
        )]);
        review.groups[0].candidates[0].0.mapping.pairs[0] = Some(99);
        let html = render_release(&view(&review, Some("tok")));
        assert!(
            html.contains("<td>—</td><td class=\"muted\">not mapped</td>"),
            "{html}"
        );
    }

    #[test]
    fn an_out_of_date_card_orders_its_notes_like_the_command_line() {
        let mut review = review_of(vec![group(
            GroupState::OutOfDate,
            vec![(exact(), vec!["blocked".into()])],
        )]);
        review.groups[0].candidates[0]
            .1
            .warnings
            .push("warned".into());
        let html = render_release(&view(&review, Some("tok")));
        let at = |needle: &str| {
            html.find(needle)
                .unwrap_or_else(|| panic!("{needle} in {html}"))
        };
        let order = [
            at("evidence: "),
            at("blocker: blocked"),
            at("warning: warned"),
        ];
        assert!(order.is_sorted(), "{order:?} in {html}");
    }
}
