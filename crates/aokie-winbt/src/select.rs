//! The choices the native engine makes that are plain logic: which audio
//! endpoint is the phone's, which call "reject or hang up" acts on, which
//! history entry names a ringing caller. Kept apart from the WinRT and WASAPI
//! calls that feed them, and not Windows-only, so they are tested on any
//! system.

/// The phone's hands-free audio endpoint among the active ones, given as
/// (friendly name, device id) pairs; its index.
///
/// Only an endpoint that is hands-free and that the phone owns. Never one
/// where Windows is the audio gateway ("Headset (WH-1000XM4 Hands-Free AG
/// Audio)": a headset of the person's own, which Windows serves as a phone
/// serves Aokie), and never a headset that is not Bluetooth hands-free at all
/// (a USB "Jabra Headset"). With the phone's name known, the endpoint must
/// carry it ("Headset (Pixel 8 Hands-Free)"). Without it only a lone
/// candidate is taken: with two, a guess could open someone's headset and
/// play the caller into the person's ears.
pub(crate) fn phone_hands_free_endpoint(
    endpoints: &[(String, String)],
    phone_name: Option<&str>,
) -> Option<usize> {
    let candidates: Vec<usize> = endpoints
        .iter()
        .enumerate()
        .filter(|(_, (name, id))| is_hands_free_unit_endpoint(name, id))
        .map(|(i, _)| i)
        .collect();
    let phone_name = phone_name
        .map(|n| n.trim().to_lowercase())
        .filter(|n| !n.is_empty());
    match phone_name {
        Some(phone) => candidates
            .into_iter()
            .find(|&i| endpoints[i].0.to_lowercase().contains(&phone)),
        None if candidates.len() == 1 => candidates.first().copied(),
        None => None,
    }
}

/// A Bluetooth hands-free endpoint on which Windows is the hands-free unit
/// (the phone's side of a call). Endpoints name themselves "… Hands-Free …"
/// and their ids may carry the BTHHF service class; "AG Audio" marks the other
/// role, where Windows is the gateway for a headset.
fn is_hands_free_unit_endpoint(name: &str, id: &str) -> bool {
    let name = name.to_lowercase();
    let id = id.to_lowercase();
    let hands_free = name.contains("hands-free") || id.contains("hands-free") || id.contains("bthhf");
    hands_free && !name.contains("ag audio")
}

/// A call as "reject or hang up" sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Leg {
    Incoming,
    Talking,
    Dialing,
    Held,
}

/// What "reject or hang up" does to the call it picks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HangupVerb {
    Reject,
    End,
}

/// Which call "reject or hang up" (the dongle's AT+CHUP) acts on, by index
/// into `legs`, and how. The call being talked on comes first: with a second
/// caller waiting, the plugin's hang up is for the call it is on, and on the
/// dongle AT+CHUP ends that call too, leaving the waiting one ringing. A
/// ringing call is rejected only when nothing is being talked on (it is then
/// the plugin's current call), then our own dial is aborted, then a held call
/// is ended.
pub(crate) fn hangup_target(legs: &[Leg]) -> Option<(usize, HangupVerb)> {
    let find = |leg: Leg| legs.iter().position(|l| *l == leg);
    find(Leg::Talking)
        .map(|i| (i, HangupVerb::End))
        .or_else(|| find(Leg::Incoming).map(|i| (i, HangupVerb::Reject)))
        .or_else(|| find(Leg::Dialing).map(|i| (i, HangupVerb::End)))
        .or_else(|| find(Leg::Held).map(|i| (i, HangupVerb::End)))
}

/// One phone-history entry as the caller-id fallback reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryEntry {
    /// When the call started, in the history store's ticks.
    pub start: i64,
    /// The entry is ringing or incoming.
    pub inbound: bool,
    /// Its number; `None` when the caller withheld it.
    pub number: Option<String>,
}

/// A ringing call's number from the recent history entries, when its own
/// entry could not be found by its id: the newest inbound entry that started
/// between `earliest` and `latest`, and its number, or `None` when that entry
/// has none (the caller withheld it). Never an older entry's number: that
/// belongs to the previous caller, and a withheld caller given it would be
/// greeted, recorded and texted back as someone else.
pub(crate) fn recent_ring_number(
    entries: &[HistoryEntry],
    earliest: i64,
    latest: i64,
) -> Option<String> {
    entries
        .iter()
        .filter(|e| e.inbound && e.start >= earliest && e.start <= latest)
        .max_by_key(|e| e.start)
        .and_then(|e| e.number.clone())
}

/// What the messaging thread says when no phone is known to text from.
pub(crate) const NO_PHONE: &str = "phone not connected";

/// Whether a text the messaging thread could not send may have reached the
/// phone all the same (review 4 #3). Only two failures say it did not:
/// there was no phone to send it to, or the phone answered a request with an
/// error (it did not take the text). Any other may have come after its PUT
/// went out, with the phone's answer what was lost.
pub(crate) fn send_failure_in_doubt(reason: &str) -> bool {
    !(reason == NO_PHONE || reason.contains(": server returned 0x"))
}

/// The texts the messaging thread answered lately, each with its answer
/// (review 4 #17: Windows' own stack kept none, so a host's retry under a
/// text's id sent it twice). Handed over again, a text is given its answer
/// again and not sent, unless it failed for certain: that one never reached
/// the phone, or the phone refused it, and it is sent again.
#[derive(Default)]
pub(crate) struct TextAnswers {
    answered: std::collections::VecDeque<(String, aokie_dongle::bluetooth::BluetoothEvent)>,
}

/// How many answers are kept: a host retries within a minute or two.
const TEXT_ANSWERS_KEPT: usize = 64;

impl TextAnswers {
    /// The answer to give a text handed over again, if it is not to be sent.
    pub(crate) fn replay(&self, message_id: &str) -> Option<&aokie_dongle::bluetooth::BluetoothEvent> {
        use aokie_dongle::bluetooth::BluetoothEvent;
        self.answered
            .iter()
            .rev()
            .find(|(id, _)| id == message_id)
            .map(|(_, answer)| answer)
            .filter(|answer| {
                !matches!(answer, BluetoothEvent::SmsSendFailed { outcome_unknown: false, .. })
            })
    }

    pub(crate) fn remember(&mut self, message_id: &str, answer: aokie_dongle::bluetooth::BluetoothEvent) {
        self.answered.retain(|(id, _)| id != message_id);
        if self.answered.len() >= TEXT_ANSWERS_KEPT {
            self.answered.pop_front();
        }
        self.answered.push_back((message_id.to_string(), answer));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review 4 #17: a text handed over again is answered, not sent twice,
    /// unless it failed for certain; a failure that may have reached the
    /// phone is in doubt.
    #[test]
    fn a_text_handed_over_again_is_sent_once_unless_it_failed_for_certain() {
        use aokie_dongle::bluetooth::BluetoothEvent;
        let failed = |id: &str, outcome_unknown| BluetoothEvent::SmsSendFailed {
            message_id: id.to_string(),
            recipient_phone: "0400111222".to_string(),
            reason: "x".to_string(),
            outcome_unknown,
        };
        let mut texts = TextAnswers::default();
        assert!(texts.replay("m1").is_none(), "a new text is sent");
        texts.remember(
            "m1",
            BluetoothEvent::SmsSent {
                message_id: "m1".to_string(),
                recipient_phone: "0400111222".to_string(),
            },
        );
        assert!(matches!(texts.replay("m1"), Some(BluetoothEvent::SmsSent { .. })));
        texts.remember("m2", failed("m2", true));
        assert!(texts.replay("m2").is_some(), "in doubt: not sent again");
        texts.remember("m3", failed("m3", false));
        assert!(texts.replay("m3").is_none(), "failed for certain: sent again");
        // Its second try is what is remembered.
        texts.remember("m3", failed("m3", true));
        assert!(texts.replay("m3").is_some());
        for i in 0..TEXT_ANSWERS_KEPT {
            texts.remember(&format!("x{i}"), failed("x", true));
        }
        assert!(texts.replay("m1").is_none(), "only the last few dozen are kept");

        assert!(!send_failure_in_doubt(NO_PHONE));
        assert!(!send_failure_in_doubt("MAS PUT: server returned 0xc3 (expected one of [160])"));
        assert!(send_failure_in_doubt("RFCOMM read timed out"));
    }

    fn endpoints(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(n, i)| (n.to_string(), i.to_string()))
            .collect()
    }

    #[test]
    fn the_phones_endpoint_is_found_by_its_name() {
        let list = endpoints(&[
            ("Speakers (Realtek)", "HDAUDIO"),
            ("Headset (WH-1000XM4 Hands-Free AG Audio)", "{0.0.0.00000000}.{a}"),
            ("Headset (Pixel 8 Hands-Free)", "{0.0.0.00000000}.{b}"),
        ]);
        assert_eq!(phone_hands_free_endpoint(&list, Some("Pixel 8")), Some(2));
        assert_eq!(phone_hands_free_endpoint(&list, Some(" pixel 8 ")), Some(2));
        // Unknown name: the one hands-free-unit candidate (the AG headset never counts).
        assert_eq!(phone_hands_free_endpoint(&list, None), Some(2));
        assert_eq!(phone_hands_free_endpoint(&list, Some("")), Some(2));
    }

    #[test]
    fn the_persons_own_headset_is_never_opened() {
        let list = endpoints(&[
            ("Headset (WH-1000XM4 Hands-Free AG Audio)", "{0.0.0.00000000}.{a}"),
            ("Jabra Headset", "USB\\VID_0B0E"),
            ("Microphone Array", "USB\\VID"),
        ]);
        assert_eq!(phone_hands_free_endpoint(&list, None), None);
        assert_eq!(phone_hands_free_endpoint(&list, Some("WH-1000XM4")), None);
        assert_eq!(phone_hands_free_endpoint(&list, Some("Pixel 8")), None);
    }

    #[test]
    fn another_phones_endpoint_is_not_the_paired_phones() {
        let list = endpoints(&[
            ("Headset (Galaxy S23 Hands-Free)", "x"),
            ("Headset (Pixel 8 Hands-Free)", "y"),
        ]);
        assert_eq!(phone_hands_free_endpoint(&list, Some("Pixel 8")), Some(1));
        assert_eq!(phone_hands_free_endpoint(&list, Some("iPhone")), None);
        // Two phones and no name to tell them apart: no guess.
        assert_eq!(phone_hands_free_endpoint(&list, None), None);
    }

    #[test]
    fn an_id_naming_bthhf_counts_as_hands_free() {
        let list = endpoints(&[("whatever", "{guid}#BTHHF#9&1234")]);
        assert_eq!(phone_hands_free_endpoint(&list, None), Some(0));
        assert_eq!(phone_hands_free_endpoint(&list, Some("Pixel 8")), None);
    }

    #[test]
    fn hang_up_with_a_caller_waiting_ends_the_call_being_talked_on() {
        assert_eq!(
            hangup_target(&[Leg::Incoming, Leg::Talking]),
            Some((1, HangupVerb::End))
        );
        assert_eq!(
            hangup_target(&[Leg::Talking, Leg::Incoming]),
            Some((0, HangupVerb::End))
        );
    }

    #[test]
    fn a_lone_ring_is_rejected_and_the_rest_ended() {
        assert_eq!(hangup_target(&[Leg::Incoming]), Some((0, HangupVerb::Reject)));
        assert_eq!(
            hangup_target(&[Leg::Held, Leg::Incoming]),
            Some((1, HangupVerb::Reject))
        );
        assert_eq!(
            hangup_target(&[Leg::Held, Leg::Dialing]),
            Some((1, HangupVerb::End))
        );
        assert_eq!(hangup_target(&[Leg::Held]), Some((0, HangupVerb::End)));
        assert_eq!(hangup_target(&[]), None);
    }

    fn entry(start: i64, inbound: bool, number: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            start,
            inbound,
            number: number.map(str::to_string),
        }
    }

    #[test]
    fn a_withheld_caller_is_not_given_the_previous_callers_number() {
        // The previous caller rang at 100 and hung up; the withheld caller rang at 120.
        let entries = [entry(100, true, Some("+15551230001")), entry(120, true, None)];
        assert_eq!(recent_ring_number(&entries, 90, 130), None);
        // Even when the withheld caller's own entry is not in the store yet, an entry
        // from before this call was seen is never taken.
        let entries = [entry(100, true, Some("+15551230001"))];
        assert_eq!(recent_ring_number(&entries, 115, 130), None);
    }

    #[test]
    fn the_ringing_callers_own_entry_names_them() {
        let entries = [
            entry(100, true, Some("+15551230001")),
            entry(121, true, Some("+15551230002")),
            entry(125, false, Some("+15551230003")), // outbound: not a ring
        ];
        assert_eq!(
            recent_ring_number(&entries, 115, 130).as_deref(),
            Some("+15551230002")
        );
        assert_eq!(recent_ring_number(&entries, 115, 120), None);
    }
}
