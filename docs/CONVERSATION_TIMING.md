# Conversation timing

In OAIY, open **AI Receptionist → Settings → Conversation tuning**.
Reconnect Aokie after changing these settings.

- **Listen while speaking and allow interruptions** (`bargeIn`) keeps the
  echo-cancelled microphone active during replies. Caller audio is retained
  with pre-roll so the first word is available to transcription.
- **Interruption threshold** (`bargeSensitivity`) controls the required
  cleaned microphone level. Lower values make interruption easier but can
  react to background noise. Start around 550–650.
- **Wait after the caller pauses** (`sttEndpointMs`) determines when an
  utterance is sent for final transcription. This is separate from the
  interruption detector; making it very short can split names and numbers.
- **Let Aokie briefly acknowledge longer explanations**
  (`conversationAcknowledgements`, off by default) adds a short “Mm-hm”
  during a small pause in a longer explanation. It requires listening while
  speaking. The caller's turn stays open, and overlap audio is appended in
  order. Acknowledgements are at least 20 seconds apart and are skipped
  during caller-requested silence, manager calls, human takeover and the
  separate Desktop Realtime responder.

The acoustic interruption gate uses a 100 ms settling window followed by
140 ms of sustained speech above the threshold. These are detector timings,
not a guarantee of end-to-end phone latency. Echo cancellation, microphone
level, Bluetooth buffering and the telephone connection also affect timing.
Brief noise bursts do not take the floor. Explicit spoken stop/wait commands
continue to override protected speech; number/important spans otherwise have
the configured bounded finish budget.

Acknowledgements are listening cues, not confirmation of a booking or another
action. They use the existing speech service and are logged as
`aokie.call.turn.final` bot turns with `kind: backchannel` and delivery status.
They do not generate a substantive answer from an unfinished transcript.
Caller words spoken over the acknowledgement remain part of their open turn.

## Spoken dates

Calendar dates such as `2026-09-14` are rendered for speech as “Monday the
fourteenth of September.” The weekday is calculated from the date, and years
other than the current year are retained. This happens before phone/code
digit expansion and applies to normal, slow and important speech. Booking
records and transcript source text retain the original date. Invalid dates,
identifiers, URLs and ambiguous numeric formats are not guessed or rewritten.

Clock times are also rendered as words: `10:05 AM` becomes “ten oh five AM,”
and `14:30` becomes “two thirty PM.” On-the-hour zero minutes are omitted.
`00:00` and `12:00 AM` are “midnight”; `12:00 PM` is “noon.” Non-zero seconds
are retained when supplied. Bare times such as `9:30` do not acquire an
invented AM/PM. Invalid times and timestamp/identifier fragments stay unchanged.

## Local checks

Run `cargo test -p aokie-plugin --lib --features voice` on Windows with the
voice build prerequisites. The synthetic audio tests cover early/short caller
interruptions, retained pre-roll, short noise, echo-only playback, protected
spans, spoken stop commands and urgent call controls. The acknowledgement
policy tests cover pauses, cooldown, stale transcripts and unfinished details.
Also run `node crates/aokie-plugin/tests/settings_ui_harness.cjs` and the
cross-repository contract check documented in `scripts/check-contracts.mjs`.

For a live check, interrupt a reply with “Wait, I meant Friday afternoon,”
then finish the sentence. Check the transcript and whether Aokie yields.
During a longer explanation, briefly pause and resume across an acknowledgement;
check that the continuation appears after the earlier words in the transcript.
