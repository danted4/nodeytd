import ytdl from '@distube/ytdl-core';
import inquirer from 'inquirer';
import fs from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';
import ffmpeg from 'fluent-ffmpeg';
import cliProgress from 'cli-progress';
import util from 'util';
import { pipeline } from 'stream';
import { promisify } from 'util';

const pipelinePromise = promisify(pipeline);
const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

/**
 * getVideoInfo - gets info about a YouTube video
 * @param {string} url - video URL
 * @returns {Promise<object>}
 */
async function getVideoInfo(url) {
    try {
        const info = await ytdl.getInfo(url);
        return info;
    } catch (error) {
        console.error('Error fetching video info:', error);
        process.exit(1);
    }
}

/**
 * downloadStream - downloads a stream with a progress indicator
 * @param {string} url - video or audio URL
 * @param {*} format - selected format
 * @param {string} filePath - output file path
 * @returns {Promise<void>}
 */
async function downloadStream(url, format, filePath) {
    return new Promise((resolve, reject) => {
        const progressBar = new cliProgress.SingleBar({
            format: `Downloading |{bar}| {percentage}% || {value}/{total} MB || {eta}s`,
            barCompleteChar: '\u2588',
            barIncompleteChar: '\u2591',
            hideCursor: true,
        }, cliProgress.Presets.shades_classic);

        const fileStream = fs.createWriteStream(filePath);
        const videoStream = ytdl(url, { format: format });

        videoStream
            .on('data', chunk => {
                if (format.contentLength) {
                    progressBar.increment(chunk.length);
                }
            })
            .on('end', () => {
                progressBar.stop();
                resolve();
            })
            .on('error', error => {
                progressBar.stop();
                reject(error);
            })
            .pipe(fileStream);

        if (format.contentLength) {
            progressBar.start(parseInt(format.contentLength, 10), 0);
        }
    });
}

/**
 * mergeFiles - merges video and audio files using ffmpeg with progress
 * @param {string} videoPath - path to video file
 * @param {string} audioPath - path to audio file
 * @param {string} outputPath - path to output file
 * @returns {Promise<void>}
 */
async function mergeFiles(videoPath, audioPath, outputPath) {
    return new Promise((resolve, reject) => {
        const progressBar = new cliProgress.SingleBar({
            format: 'Merging |{bar}| {percentage}% || {value}/{total} MB || {eta}s',
            barCompleteChar: '\u2588',
            barIncompleteChar: '\u2591',
            hideCursor: true,
        }, cliProgress.Presets.shades_classic);

        // Get the file size of the video and audio files to estimate progress
        const videoSize = fs.statSync(videoPath).size;
        const audioSize = fs.statSync(audioPath).size;
        const totalSize = videoSize + audioSize;

        let processedSize = 0;

        ffmpeg()
            .input(videoPath)
            .input(audioPath)
            .audioCodec('aac')
            .videoCodec('copy')
            .output(outputPath)
            .on('progress', (progress) => {
                const processed = (progress.percent / 100) * totalSize;
                processedSize = Math.min(processed, totalSize);
                progressBar.update(processedSize);
            })
            .on('end', () => {
                progressBar.stop();
                fs.unlinkSync(videoPath);
                fs.unlinkSync(audioPath);
                console.log(`Merged to ${outputPath}`);
                resolve();
            })
            .on('error', (err) => {
                progressBar.stop();
                reject(err);
            })
            .run();
    });
}

/**
 * main - main function
 * @returns {Promise<void>}
 */
async function main() {
    const urlQuestion = {
        type: 'input',
        name: 'url',
        message: 'Enter the YouTube video URL:',
        validate: value => ytdl.validateURL(value) ? true : 'Invalid URL!'
    };

    const { url } = await inquirer.prompt(urlQuestion);
    const info = await getVideoInfo(url);
    const mp4 = 'mp4';
    const videoFormats = info.formats.filter(format => format.hasVideo && format.container === mp4);
    const audioFormats = info.formats.filter(format => format.hasAudio && format.container === mp4);

    const videoChoices = videoFormats.map(format => ({
        name: format.qualityLabel ? `${format.qualityLabel} - ${format.container}` : `${format.itag} - ${format.container}`,
        value: format.itag
    }));

    const audioChoices = audioFormats.map(format => ({
        name: format.audioBitrate ? `${format.audioBitrate}kbps - ${format.container}` : `${format.itag} - ${format.container}`,
        value: format.itag
    }));

    const videoQuestion = {
        type: 'list',
        name: 'videoFormat',
        message: 'Choose the video resolution:',
        choices: videoChoices
    };

    const audioQuestion = {
        type: 'list',
        name: 'audioFormat',
        message: 'Choose the audio quality:',
        choices: audioChoices
    };

    const { videoFormat } = await inquirer.prompt(videoQuestion);
    const { audioFormat } = await inquirer.prompt(audioQuestion);

    const selectedVideoFormat = info.formats.find(f => f.itag === videoFormat);
    const selectedAudioFormat = info.formats.find(f => f.itag === audioFormat);

    if (selectedVideoFormat && selectedAudioFormat) {
        const dt = new Date();
        const padZero = (num) => num.toString().padStart(2, '0');
        const year = dt.getFullYear();
        const month = padZero(dt.getMonth() + 1);
        const day = padZero(dt.getDate());
        const hours = padZero(dt.getHours());
        const minutes = padZero(dt.getMinutes());
        const seconds = padZero(dt.getSeconds());
        const sanitizedTitle = info.videoDetails.title.replace(/[^a-zA-Z0-9]/g, '-');

        const videoPath = path.join(__dirname, `downloads/${sanitizedTitle}-${year}-${month}-${day}-${hours}-${minutes}-${seconds}-video.${selectedVideoFormat.container}`);
        const audioPath = path.join(__dirname, `downloads/${sanitizedTitle}-${year}-${month}-${day}-${hours}-${minutes}-${seconds}-audio.${selectedAudioFormat.container}`);
        const outputPath = path.join(__dirname, `downloads/${sanitizedTitle}-${year}-${month}-${day}-${hours}-${minutes}-${seconds}.${selectedVideoFormat.container}`);

        await downloadStream(url, selectedVideoFormat, videoPath);
        await downloadStream(url, selectedAudioFormat, audioPath);
        await mergeFiles(videoPath, audioPath, outputPath);
    } else {
        console.log('Selected formats not available.');
    }
}

main().catch(err => {
    console.error('Error:', err);
});
