# warm-me

Keep your laptop warm in the cold by turning spare CPU into heat — without
getting in the way of what you're actually doing.

```
warm-me              # hold the battery at 35°C
warm-me 38           # different target (battery targets cap at 40°C)
warm-me -m 50        # gentler: never more than 50% heater duty
warm-me -p           # only heat while plugged in
warm-me --list       # show every temperature sensor it can see
```

Runs on macOS, Linux and Windows (Intel, AMD and Apple Silicon).

## How it works

**Sensor.** Regulates on battery temperature by default: the pack sits under
most of the chassis, so it tracks how warm the laptop feels, and it's the part
that suffers most in the cold. Machines without a battery sensor fall back to
the hottest CPU die sensor (`--sensor cpu`, default target 55°C).

**Heater.** One worker per core runs wide floating-point math plus streaming
memory copies, which draws more power than an idle spin. Each worker is
duty-cycled in 10 ms slices, so heat output is continuous from 0 to 100%
rather than on/off.

**Controller.** A PI loop sets the duty from the temperature gap. Battery
temperature lags the heater by minutes, so the loop acts on where the
temperature is *heading* (10 minutes out at the current rate) and only learns
extra steady-state power when progress stalls. That's what lets it find the
right output for any machine in any room without overshooting: in simulation
it settles on target with 0.2–0.4°C overshoot (`cargo test`).

**Staying out of your way.**
- Workers run at the lowest priority the OS offers: `SCHED_IDLE` on Linux,
  utility QoS on macOS, idle priority on Windows.
- Priority alone isn't enough: on modern chips, busy cores pull down shared
  clocks and power budget for everyone. So four times a second warm-me checks
  for programs using a real share of a core, and steps aside while they run —
  their work is making heat anyway. It returns gradually once they finish.
  In testing, a CPU-bound job ran 3% slower alongside warm-me, versus 2–3×
  slower next to a naive busy loop. Use `--no-yield` to heat regardless.
- Backs off if the CPU passes 95°C (`--cpu-limit`).

**Battery.** Every watt of heat comes out of the battery or the charger;
software can't change that. warm-me only makes the heat needed to hold the
target, pauses on battery below 20% (`--min-battery`), and `-p` restricts it to
wall power. It warns if the battery is charging below 0°C, which damages
lithium cells: warm up first, then plug in.

## Install

```
cargo install --git https://github.com/AnirudhG07/warm-me
```

or from a clone: `cargo install --path .`
