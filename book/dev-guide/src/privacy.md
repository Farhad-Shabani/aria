{{#title Speech library privacy and recordings - Using Aria Libraries}}

# Privacy

Whisper, Kokoro, and the system voice run on your machine and make no network
requests. Only builds and model downloads use the network. Your code decides
what to save or send. The Aria app can also run recognition on a server
you pair; the user guide's privacy page covers that.

## Recordings

Capture keeps each recording in memory and writes no file. A recording holds
everything the microphone heard, including background voices and about one
second from before `record()`. Each reader holds the audio it hasn't
released, about 230 MB per hour. Dropping the recording frees it.

## Your files and output

`transcribe_file` reads your WAV and leaves it in place. Transcripts and
generated audio stay in memory until your code saves them.

## Model files

Use model files from sources you trust, and check their licenses before
shipping. Native loaders require an absolute path, current-user or root
ownership, and a file that group and others cannot write. These checks do not
isolate engine code.
