# README screenshots

Pictures used by the repository's [README](../../README.md). Every name, number and
record in them is fictional.

## The receptionist screen (29 September 2026)

The plugin's own screen, `crates/aokie-plugin/ui/receptionist`, loaded in headless
Chrome the way OAIY Desktop builds a plugin screen: the same document, CSP, host fonts
and theme attribute. A stub `PluginHost` answers its commands instead of a running
plugin, so no phone, dongle or network was involved. The data is fictional: the
business "Green Lawns", the ACMA fictional numbers 0491 570 006 and +61 491 570 156,
and Bluetooth addresses from the documentation range `00:00:5E:00:53:xx`.

| File | What it shows |
|---|---|
| `receptionist-overview-light.png`, `receptionist-overview-dark.png` | Normal mode, the Overview tab: ready for calls, calls going to OAIY's Front desk, a live call with a second caller waiting, the paired phone and data delivery. |
| `receptionist-setup-pairing-light.png`, `receptionist-setup-pairing-dark.png` | Setup mode: the "Pair your phone" step as OAIY's setup wizard shows it, with no tab bar, holding a numeric-comparison code for the person to confirm. Only the step's pane is shown; OAIY draws the wizard around it. |

These replace `oaiy-receptionist-live.png`, a capture of an older version of the
screen.

## The FormLogic front desk (12 September 2026)

`front-desk-demo-desktop.png`, `appointments-demo-desktop.png` and
`front-desk-demo-mobile.png` show the actual running FormLogic app and its hosted Softn
Aokie template. The browser received fictional demo records for the screenshots; no
server records or settings were changed.

## Older illustrations

`aokie-hero.png`, `aokie-local-first.png`, `call-journey.svg` and
`responsibility-map.svg` are earlier illustrations. No page links to them now, and
`responsibility-map.svg` still names FormLogic Desktop as the only host. They are kept
for the history.
