# nodeYTD
YTD Node Client wrapper on @distube/ytdl-core

## Prerequisite

- Node.js `26+` (or `22.22.2+` on 22.x, `24.15.0+` on 24.x), required by `@distube/ytdl-core` v5
- ffmpeg (must be installed and available in PATH)

## STEPS

### 1. Install ffmpeg from https://ffmpeg.org/download.html and add the bin path in environment variables
### 2. Download this code on your local machine.
### 3. Open a command prompt in the `ytd` folder.
### 4. RUN `npm install`
### 5. RUN `npm start`
### 6. A prompt will ask you for the YouTube URL.
### 7. Paste it, hit enter.
### 8. Choose what you want to download:
	- Audio Only (mp3)
	- Video Only
	- Both (merged mp4)
	- Single file (best available, e.g. webm/mp4)
### 9. Select the desired quality/format from the list.
### 10. Optionally, enter trim start/end times for the output file. You can use flexible formats:
	- `3` or `03` for 3 seconds
	- `3:00` for 3 minutes
	- `1:02:03` for 1 hour, 2 minutes, 3 seconds
	- Leave blank for full file
### 11. Your file will be available in the `downloads` sub-directory of this project.

## Features

- Download audio only and convert to mp3
- Download video only (no audio)
- Download both video and audio, merged to mp4 (only compatible formats shown)
- Download best available single file (video+audio, e.g. webm/mp4)
- Optionally trim output file with flexible time input

## Usage

Run `npm start` and follow the prompts:

1. Enter the YouTube video URL
2. Choose download type (audio, video, both, or best available)
3. Select quality/format
4. Find your file in the `downloads` folder

## Star History

[![Star History Chart](https://api.star-history.com/svg?repos=danted4/nodeytd&type=Date)](https://star-history.com/#danted4/nodeytd&Date)
