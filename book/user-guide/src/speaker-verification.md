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
**Settings → Recognition → Only type my voice**. It is off by default. The
CLI cannot change it, but CLI dictation follows it.

Aria rejects speech judged to be another voice. When there is too little
usable audio to identify the speaker, it keeps the words without claiming
an owner match. Without an enrolled owner, dictation fails instead of
typing every voice.

Filtering is not a privacy guard. Short replies from other people can pass,
and recognition can still miss words. Recordings kept for
[retry](./history.md#retry) still hold every voice.

## See who is speaking

While you dictate, the waves in the status pill show whether Aria is typing
the voice it hears. They brighten for a voice it types and fade for one it
excludes. Until Aria matches a voice, including short speech it keeps
without an identity match, the waves stay at their usual red. A lost
connection returns them to red until fresh voice information arrives.

## Other speakers

To add a guest:

```sh
aria speaker enroll partner \
  --role guest
```

To make a guest the owner, choose **Make Owner** from its menu in
**Settings → Recognition**, or run:

```sh
aria speaker owner partner
```

The previous owner becomes a guest.

In system-audio transcripts, Listen names a voice with a profile once it
is sure who is speaking. Other voices get numbers, such as Speaker 1.
Labels can change as more audio arrives, so the saved transcript may differ
from what the terminal printed first.

## Replace or remove

In the Mac app, each voice in **Settings → Recognition** has a menu with
**Set Up Again…** and **Remove…**.

In the terminal, to replace `alex` with new recordings:

```sh
aria speaker enroll alex \
  --replace
```

A replaced profile keeps its role.

To remove a profile, use its name from the list:

```sh
aria speaker remove partner
```

Profiles are stored locally and contain a numeric representation of the voice.
If enrollment fails, check the speaker-model path with `aria doctor` and use
[Troubleshooting](./troubleshooting.md).
