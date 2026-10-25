{{#title Set up voice profiles on Mac - Aria User Guide}}

# Voice profiles (Mac)

This chapter is for Mac users. Enabling dictation filtering needs the Mac app,
and speaker labels are used by Mac system-audio transcription.

A voice profile gives Aria a name for a speaker. An **owner** profile can also
help it select your speech during dictation; a **guest** supplies labels for
[system-audio transcripts](./listen.md).

This is a recognition aid, not authentication. Filtering can allow other
voices through when it cannot make a confident match.

## Enroll

Complete [setup](./installation.md#setup), accept the speaker-model download,
and keep the background service running. Allow microphone access when asked.

In the Mac app, open **Settings → Recognition** and click **Set Up…** under
**Voice**. Enter your name, click **Start**, and read each sentence aloud.
Pause after each one; the pause tells Aria the sentence is done. A voice
added while there is no owner becomes the owner. Other voices are guests; add
one with **Add Voice…**.

In the terminal, to create an owner profile named `alex`:

```sh
aria speaker enroll alex \
  --role owner
```

Follow the prompts and provide clear recordings from one speaker. Enrollment
asks for five utterances by default and checks that they agree before saving.
After it succeeds, run:

```sh
aria speaker list
```

It shows each profile's name and role.

## Use your profile

Enrollment does not turn filtering on. In the Mac app, enable
**Settings → Recognition → Only type my voice**.
It is off by default, and the CLI has no control for this preference.

Filtering needs the speaker model and an owner profile. Without the model,
live dictation passes all speech through, but a retry reports no speech.
Without an owner profile, or when a sample is inconclusive, speech can pass
through. Do not rely on it to
keep private background speech out of a recording. Retained originals can
contain voices excluded from the delivered text.

## Other speakers

To add a guest:

```sh
aria speaker enroll partner \
  --role guest
```

Voice profiles can supply speaker labels in system-audio transcripts. Labels
may change as more audio arrives, so the saved transcript may differ from the
first labels printed in the terminal.

## Replace or remove

In the Mac app, each voice in **Settings → Recognition** has a menu with
**Set Up Again…** and **Remove…**.

In the terminal, to replace `alex` with new recordings, keep its role explicit:

```sh
aria speaker enroll alex \
  --role owner \
  --replace
```

To remove a profile, use its name from the list:

```sh
aria speaker remove partner
```

Profiles are stored locally and contain a numeric representation of the voice.
If enrollment fails, check the speaker-model path with `aria doctor` and use
[Troubleshooting](./troubleshooting.md).
