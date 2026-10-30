# warm-me

> Warning! Stupid but a very "guy nerd" idea ahead. Use at your own risk.

Get this... You are coding(or these days, vibe coding) using your laptop keyboard, but it is soo cold around you that you wish you could just touch something warm. What do you do???

**Options**:

1. Be in a better and warmer workplace(could be a buying a heater or better air conditioning)... NAH!
2. Wear gloves while coding... makes it hard and you look ridiculous while working.
3. Warm your laptop! and feel the warmth from your laptop on yourself. THAT's IT!

Have you never thought about it? When your laptop is running heavy stuff, it gets warm. You worry about your laptop getting hot instead of taking the warmth from it. Even your partner doesn't give you this much instant warmth.

## Install

```bash
eget AnirudhG07/warm-me
```

or grab the one for your machine from [releases](https://github.com/AnirudhG07/warm-me/releases) (Linux, macOS, Windows, Intel and ARM), unpack, and put `warm` somewhere on your PATH. Or build it yourself:

```bash
cargo install --git https://github.com/AnirudhG07/warm-me
```

## How to use

```bash
warm        # start heating and watch it live (↑↓ change temp, q to leave)
warm on     # start heating in the background
warm off    # stop it
warm help   # all the other options
```

## FAQ's

These are some Frequently Asked Questions, which technically have never been asked, but still hypothetically, you might be asking them.

**Question**: How does it work?

We tap into your laptop's temperature sensors (battery one mostly, it sits under your palms so it's closest to what you feel) and read them every second. Too cold? we burn more CPU and GPU. Warm enough? we chill and just keep it there.

**Question**: How hot does it get?

35°C by default, measured at the battery. Max is 40°C, past that it's not good for the battery. You'll feel the laptop getting warm in a few minutes, the battery number itself moves slower.

**Question**: How do I make it hotter or cooler?

`warm 38` to aim for 38°C. Or just hit ↑ ↓ while it's running, 0.5°C a press. ← → changes how hard it's allowed to go.

**Question**: Does it affect my laptop's performance?

We try not to mess with your work much by running at usually the lowest priority, and backing off whenever you're doing something heavy yourself. Your stuff makes heat anyway lol. Only when warming up from cold does it go all in for a bit. But in short, YES, it will affect your laptop's performance, but not much.

**Question**: Can it do something useful instead of burning CPU for nothing?

Yes! Lend your laptop to real science and get warm doing it:

```bash
warm --work primes    # GIMPS, hunt for record primes (mersenne.org)
warm --work boinc     # BOINC, Rosetta@home, Einstein@home, World Community Grid...
warm --work folding   # Folding@home, protein stuff for disease research
warm --pid 1234       # any program you already have running
warm --run "cmd"      # or start one
```

You install and sign up for the project once (warm tells you how if it's missing), then warm runs it just enough to hold your temperature. If it doesn't use every core, warm tops up the rest. If it stops, warm goes back to its own heat.

**Question**: Will it eat my battery?

Yep, heat = power, no way around it. It stops by itself below 20% battery, or use `warm on -p` to only heat when plugged in.

**Question**: Is it safe for my laptop?

Well... It is technically running a heavy workload on your laptop. Hardware these days support long running heavy processes. So as long as you don't run it for too long, it should be fine. Your chip slows itself down way before anything bad, and warm never pushes the battery past 40°C. It even warns you if you're charging below 0°C (bad for batteries, warm up first, then plug in).
