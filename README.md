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
cargo install --git https://github.com/AnirudhG07/warm-me
```

## How to use

```bash
warm        # start heating and watch it live (↑↓ change temp, q to leave)
warm on     # start heating in the background
warm off    # stop it
warm help   # all the other options
```

## How it works

We tap into your laptop's temperature sensors (battery one mostly, it sits under your palms so it's closest to what you feel) and read them every second. Too cold? we burn more CPU and GPU. Warm enough? we chill and just keep it there.

We try not to mess with your work much by running at the lowest priority, and backing off whenever you're doing something heavy yourself. Your stuff makes heat anyway lol.
