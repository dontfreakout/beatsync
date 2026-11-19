# Media fixtures

Each file is a 2.0 s, 48 kHz, mono 440 Hz sine. `tone.wav` is 192 KB because raw 16-bit PCM cannot be smaller. Generated with ffmpeg:

```sh
S="-f lavfi -i sine=frequency=440:sample_rate=48000:duration=2"
ffmpeg $S -c:a libmp3lame -b:a 32k tone.mp3
ffmpeg $S -c:a aac -b:a 32k tone.m4a
ffmpeg $S -c:a flac tone.flac
ffmpeg $S -c:a pcm_s16le tone.wav
ffmpeg $S -c:a libvorbis -b:a 32k tone.ogg
ffmpeg $S -c:a alac tone-alac.m4a
ffmpeg $S -c:a libopus -b:a 32k tone.opus
ffmpeg $S -c:a libopus -b:a 32k tone.webm
# WebM with no duration in the header (forces the packet scan):
ffmpeg $S -c:a libopus -b:a 32k -f webm -live 1 tone-live.webm
# 300 bytes of tone.mp3, then 4000 zero bytes. The header still promises 2.0 s.
(head -c 300 tone.mp3; head -c 4000 /dev/zero) > broken.mp3
```
