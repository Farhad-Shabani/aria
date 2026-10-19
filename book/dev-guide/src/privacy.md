# Privacy

Whisper, Kokoro, and the system voice run on your machine and make no network
requests. Only builds and model downloads use the network. Your code decides
what to save or send.

## Temporary recordings

Microphone capture writes each recording to a private WAV in `aria-<uid>/`
under the system temporary folder (`$TMPDIR`, or `/tmp`). The folder is `0700`
and the file `0600`. It holds everything the microphone heard, including
background voices and about one second from before `record()`. It grows up
to the microphone's maximum duration, about 115 MB per hour.

Dropping the recording deletes the file. If your process crashes or is
killed, the file stays until you or the system remove it. Deletion is not
secure erasure and cannot remove snapshots or backups.

## Your files and output

`transcribe_file` reads your WAV and leaves it in place. Transcripts and
generated audio stay in memory until your code saves them.

## Model files

Use model files from sources you trust, and check their licenses before
shipping. Native loaders require an absolute path, current-user or root
ownership, and a file that group and others cannot write. These checks do not
isolate engine code.
