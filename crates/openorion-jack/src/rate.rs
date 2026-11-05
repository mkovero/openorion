//! Rate guard (DESIGN.md sec. 9 item 2): compare the JACK rate with the
//! Orion's and decide what to do. Pure; the caller logs and sends ops.

use openorion_proto::SampleRate;
use openorion_proto::names::sample_rate_name;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateAction {
    /// Rates agree (or nothing to compare yet).
    Ok,
    /// Mismatch; only warn.
    Warn(String),
    /// Mismatch and `--follow-jack`: set the Orion to this rate.
    SetOrion(SampleRate, String),
}

/// Decide for one comparison. `orion` None = unknown (no state yet or an
/// unknown rate nibble). `already_requested` is the rate this process last
/// asked the Orion for, so a pending change is not re-sent.
pub fn check(
    jack_hz: u32,
    orion: Option<SampleRate>,
    follow: bool,
    already_requested: Option<SampleRate>,
) -> RateAction {
    let Some(o) = orion else {
        return RateAction::Ok;
    };
    if o.hz() == jack_hz {
        return RateAction::Ok;
    }
    let base = format!(
        "sample rate mismatch: JACK runs at {jack_hz} Hz, the Orion at {} ({} Hz)",
        sample_rate_name(o),
        o.hz()
    );
    if !follow {
        return RateAction::Warn(format!(
            "{base}. Restart JACK at {} Hz or change the Orion's rate \
             (openorion-jack --follow-jack does it automatically).",
            o.hz()
        ));
    }
    match SampleRate::from_hz(jack_hz) {
        None => RateAction::Warn(format!(
            "{base}. --follow-jack: the Orion has no {jack_hz} Hz mode."
        )),
        Some(r) if already_requested == Some(r) => RateAction::Warn(format!(
            "{base}. Already asked the Orion for {jack_hz} Hz; waiting for it."
        )),
        Some(r) => RateAction::SetOrion(
            r,
            format!("{base}. --follow-jack: setting the Orion to {jack_hz} Hz."),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions() {
        assert_eq!(
            check(48_000, Some(SampleRate::R48k), true, None),
            RateAction::Ok
        );
        assert_eq!(check(48_000, None, true, None), RateAction::Ok);
        assert!(matches!(
            check(48_000, Some(SampleRate::R96k), false, None),
            RateAction::Warn(m) if m.contains("48000 Hz") && m.contains("96k")
        ));
        assert!(matches!(
            check(48_000, Some(SampleRate::R96k), true, None),
            RateAction::SetOrion(SampleRate::R48k, _)
        ));
        assert!(matches!(
            check(48_000, Some(SampleRate::R96k), true, Some(SampleRate::R48k)),
            RateAction::Warn(_)
        ));
        assert!(matches!(
            check(22_050, Some(SampleRate::R96k), true, None),
            RateAction::Warn(m) if m.contains("no 22050 Hz mode")
        ));
    }
}
