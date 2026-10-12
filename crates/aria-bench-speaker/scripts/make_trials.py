"""Build a verification trial list from a LibriSpeech split.

LibriSpeech ships no verification protocol, so one is generated: every
speaker enrolls from a few utterances, is tested against several more of
their own, then against as many strangers. Pairs are drawn with a fixed
seed, so a rerun asks the same questions.
"""

import argparse
import random
import re
from pathlib import Path


def utterances(root: Path) -> dict[str, list[Path]]:
    """Every speaker's WAVs, keyed by speaker id."""
    speakers: dict[str, list[Path]] = {}
    for wav in sorted(root.rglob("*.wav")):
        # LibriSpeech names files <speaker>-<chapter>-<n>.wav; the VoxCeleb
        # mirror flattens <speaker>/<video>/<n>.wav with plus signs. Either
        # way the speaker is what comes before the first separator.
        speaker = re.split(r"[-+]", wav.name, maxsplit=1)[0]
        speakers.setdefault(speaker, []).append(wav)
    return speakers


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("root", type=Path)
    parser.add_argument("out", type=Path)
    parser.add_argument("--enroll", type=int, default=3)
    parser.add_argument("--per-speaker", type=int, default=10)
    parser.add_argument(
        "--mix",
        type=int,
        default=0,
        help="how many enrollment recordings come from a different speaker",
    )
    parser.add_argument("--seed", type=int, default=20260908)
    arguments = parser.parse_args()

    random.seed(arguments.seed)
    speakers = utterances(arguments.root)
    usable = {
        name: files
        for name, files in speakers.items()
        if len(files) >= arguments.enroll + arguments.per_speaker
    }
    if not usable:
        raise SystemExit(f"no speaker in {arguments.root} has enough recordings")

    lines = [
        f"# {arguments.root.name}: {len(usable)} speakers, "
        f"{arguments.enroll} recordings per enrollment, {arguments.mix} of them "
        f"another speaker's, seed {arguments.seed}"
    ]
    names = sorted(usable)
    for name in names:
        files = list(usable[name])
        random.shuffle(files)
        enrollment = files[: arguments.enroll]
        if arguments.mix:
            # Someone else spoke into some of the samples, so the profile
            # blends two voices.
            intruder = random.choice([other for other in sorted(usable) if other != name])
            enrollment[: arguments.mix] = random.sample(usable[intruder], arguments.mix)
        mine = files[arguments.enroll : arguments.enroll + arguments.per_speaker]
        side = ",".join(str(path.relative_to(arguments.out.parent)) for path in enrollment)

        for test in mine:
            lines.append(f"1 {side} {test.relative_to(arguments.out.parent)}")

        strangers = [other for other in names if other != name]
        for _ in range(arguments.per_speaker):
            other = random.choice(strangers)
            test = random.choice(usable[other])
            lines.append(f"0 {side} {test.relative_to(arguments.out.parent)}")

    arguments.out.write_text("\n".join(lines) + "\n")
    trials = len(lines) - 1
    print(f"{trials} trials over {len(usable)} speakers -> {arguments.out}")


if __name__ == "__main__":
    main()
