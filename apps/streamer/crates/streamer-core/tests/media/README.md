# Media fixtures

## Tone (track loader)

`tone.mp3` is a 2.0 s, 48 kHz, mono 440 Hz sine. `broken.mp3` does not decode. Generated with ffmpeg:

```sh
S="-f lavfi -i sine=frequency=440:sample_rate=48000:duration=2"
ffmpeg $S -c:a libmp3lame -b:a 32k tone.mp3
# 300 bytes of tone.mp3, then 4000 zero bytes. The header still promises 2.0 s.
(head -c 300 tone.mp3; head -c 4000 /dev/zero) > broken.mp3
```

## Clicks (decoding)

Each file is 1.000 s long. `click_left.wav` is the source of the `click_left.*` files: 44.1 kHz stereo,
8-bit PCM (16-bit PCM would be 176 KB). Its left channel is silent except a 2 ms click (88 samples) that
starts at full scale at sample 22050 (exactly 0.500 s) and decays. The right channel is silent. The click
in `mono_22k.wav` (22.05 kHz mono), in the centre channel of `six_ch.flac` (48 kHz 5.1) and in the left
channel of `click_left_96k.flac` (96 kHz stereo, 16-bit, the decoder resamples it down) also starts at
0.500 s. Generated with ffmpeg 8.1.3:

```sh
Q="-y -bitexact"
ffmpeg -f lavfi -i "aevalsrc='if(between(n,22050,22137),exp(-(n-22050)/18),0)|0':s=44100:d=1" -c:a pcm_u8 $Q click_left.wav
ffmpeg -i click_left.wav -c:a libmp3lame -b:a 192k $Q click_left.mp3
ffmpeg -i click_left.wav -c:a aac -b:a 192k $Q click_left.m4a
ffmpeg -i click_left.wav -c:a alac $Q click_left_alac.m4a
ffmpeg -i click_left.wav -c:a flac $Q click_left.flac
ffmpeg -i click_left.wav -c:a libvorbis -q:a 6 $Q click_left.ogg
ffmpeg -i click_left.wav -c:a libopus -b:a 128k $Q click_left.opus
ffmpeg -i click_left.wav -c:a libopus -b:a 128k $Q click_left.webm
ffmpeg -f lavfi -i "aevalsrc='if(between(n,11025,11068),exp(-(n-11025)/9),0)':s=22050:d=1" -c:a pcm_s16le $Q mono_22k.wav
ffmpeg -f lavfi -i "aevalsrc='0|0|if(between(n,24000,24095),exp(-(n-24000)/20),0)|0|0|0':s=48000:d=1:channel_layout=5.1" \
  -c:a flac -sample_fmt s16 $Q six_ch.flac
ffmpeg -f lavfi -i "aevalsrc='if(between(n,48000,48191),exp(-(n-48000)/39),0)|0':s=96000:d=1" \
  -c:a flac -sample_fmt s16 $Q click_left_96k.flac
# click_left.m4a with an explicit HE-AAC AudioSpecificConfig in its esds box: the AAC-LC config
# 12 10 56 e5 00 (44.1 kHz stereo, then the 0x2B7 extension with the SBR flag 0) becomes
# 2b 92 08 00 00 (AOT 5, 22.05 kHz core, 44.1 kHz SBR rate, AOT 2). The audio stays AAC-LC.
python3 -c "
d = open('click_left.m4a', 'rb').read()
lc = bytes.fromhex('0580808005121056e500')
assert d.count(lc) == 1
open('he_explicit.m4a', 'wb').write(d.replace(lc, bytes.fromhex('05808080052b92080000')))
"
# The first 40 % of the bytes of click_left.mp3. Its Xing header still promises 1.0 s.
head -c $(( $(stat -c %s click_left.mp3) * 40 / 100 )) click_left.mp3 > truncated.mp3
```

## Quiet starts (length estimates)

The ADTS reader always estimates the frame count of a track from the file size and the sizes of some
frames. The MP3 reader does the same when the first frame has no Xing, Info or VBRI tag. A silent start
has small frames, so the estimate is too long. Both files are valid and must play. 44.1 kHz stereo,
silence, then noise:

```sh
Q="-y -bitexact"
# 7 s, 2.5 s silent. The reader estimates 335 872 frames; the file has 310 272 (share 0.92).
ffmpeg -f lavfi -i "aevalsrc='if(lt(t,2.5),0,0.5*(random(0)-0.5))|if(lt(t,2.5),0,0.5*(random(1)-0.5))':s=44100:d=7" \
  -c:a aac -b:a 64k $Q quiet_start.aac
# 3 s, 1 s silent, no Xing tag. The reader estimates 347 904 frames; the file has 133 632 (share 0.38).
ffmpeg -f lavfi -i "aevalsrc='if(lt(t,1),0,0.5*(random(0)-0.5))|if(lt(t,1),0,0.5*(random(1)-0.5))':s=44100:d=3" \
  -c:a libmp3lame -q:a 2 -write_xing 0 $Q quiet_start_noxing.mp3
```

`../../assets/kick.wav` is a copy of the web client's metronome kick (`apps/client/public/kick.wav`).
