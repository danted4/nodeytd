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

// Helper to calculate duration from start/end in hh:mm:ss
function parseFlexibleTime(t) {
    if (!t) return 0;
    const parts = t.split(':').map(Number).reverse();
    let seconds = 0;
    if (parts.length === 1) {
        seconds = parts[0];
    } else if (parts.length === 2) {
        seconds = parts[0] + parts[1] * 60;
    } else if (parts.length === 3) {
        seconds = parts[0] + parts[1] * 60 + parts[2] * 3600;
    }
    return seconds;
}

function calcDuration(start, end) {
    if (!end) return undefined;
    const startSec = parseFlexibleTime(start);
    const endSec = parseFlexibleTime(end);
    return endSec - startSec;
}
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
    const webm = 'webm';

    // Filter formats
    const videoFormats = info.formats.filter(format => format.hasVideo && !format.hasAudio);
    const audioFormats = info.formats.filter(format => format.hasAudio && !format.hasVideo);
    const avFormats = info.formats.filter(format => format.hasVideo && format.hasAudio);

    // Download type
    const { downloadType } = await inquirer.prompt({
        type: 'list',
        name: 'downloadType',
        message: 'What do you want to download?',
        choices: [
            { name: 'Recommended (merged mp4)', value: 'both' },
            { name: 'Audio Only (mp3)', value: 'audio' },
            { name: 'Video Only', value: 'video' },
            { name: 'Single file (best available)', value: 'av' }
        ]
    });

    const dt = new Date();
    const padZero = (num) => num.toString().padStart(2, '0');
    const year = dt.getFullYear();
    const month = padZero(dt.getMonth() + 1);
    const day = padZero(dt.getDate());
    const hours = padZero(dt.getHours());
    const minutes = padZero(dt.getMinutes());
    const seconds = padZero(dt.getSeconds());
    const sanitizedTitle = info.videoDetails.title.replace(/[^a-zA-Z0-9]/g, '-');

    if (downloadType === 'audio') {
        // Audio only
        const audioChoices = audioFormats.map(format => ({
            name: `${format.audioBitrate || ''}kbps - ${format.container}`,
            value: format.itag
        }));
        const { audioFormat } = await inquirer.prompt({
            type: 'list',
            name: 'audioFormat',
            message: 'Choose the audio quality:',
            choices: audioChoices
        });
        const selectedAudioFormat = info.formats.find(f => f.itag === audioFormat);
        const defaultMp3Name = `${sanitizedTitle}-${year}-${month}-${day}-${hours}-${minutes}-${seconds}.mp3`;
        const { customName } = await inquirer.prompt({
            type: 'input',
            name: 'customName',
            message: 'Rename output file (leave blank for default):',
            default: defaultMp3Name
        });
        const audioPath = path.join(__dirname, `downloads/${sanitizedTitle}-${year}-${month}-${day}-${hours}-${minutes}-${seconds}-audio.${selectedAudioFormat.container}`);
        // Remove extension if user provided one
        let baseName = customName;
        if (baseName && baseName.toLowerCase().endsWith('.mp3')) {
            baseName = baseName.slice(0, -4);
        }
        const mp3Path = path.join(__dirname, 'downloads', `${baseName || defaultMp3Name.slice(0, -4)}.mp3`);
        await downloadStream(url, selectedAudioFormat, audioPath);
        const { trimStart, trimEnd } = await inquirer.prompt([
            {
                type: 'input',
                name: 'trimStart',
                message: 'Trim start time (hh:mm:ss, leave blank for start):',
                default: ''
            },
            {
                type: 'input',
                name: 'trimEnd',
                message: 'Trim end time (hh:mm:ss, leave blank for end):',
                default: ''
            }
        ]);
        await new Promise((resolve, reject) => {
            let cmd = ffmpeg(audioPath).toFormat('mp3');
            if (trimStart) cmd = cmd.setStartTime(trimStart);
            if (trimEnd) cmd = cmd.setDuration(calcDuration(trimStart, trimEnd));
            cmd.on('end', () => {
                fs.unlinkSync(audioPath);
                console.log(`Saved as ${mp3Path}`);
                resolve();
            })
            .on('error', reject)
            .save(mp3Path);
        });
    } else if (downloadType === 'video') {
        // Video only
        const videoChoices = videoFormats.map(format => ({
            name: `${format.qualityLabel || ''} - ${format.container}`,
            value: format.itag
        }));
        const { videoFormat } = await inquirer.prompt({
            type: 'list',
            name: 'videoFormat',
            message: 'Choose the video resolution:',
            choices: videoChoices
        });
        const selectedVideoFormat = info.formats.find(f => f.itag === videoFormat);
        const defaultVideoName = `${sanitizedTitle}-${year}-${month}-${day}-${hours}-${minutes}-${seconds}-video.${selectedVideoFormat.container}`;
        const { customName } = await inquirer.prompt({
            type: 'input',
            name: 'customName',
            message: 'Rename output file (leave blank for default):',
            default: defaultVideoName
        });
        let baseName = customName;
        if (baseName && baseName.toLowerCase().endsWith(`.${selectedVideoFormat.container}`)) {
            baseName = baseName.slice(0, -(selectedVideoFormat.container.length + 1));
        }
        const videoPath = path.join(__dirname, 'downloads', `${baseName || defaultVideoName.replace(/\.[^.]+$/, '')}.${selectedVideoFormat.container}`);
        await downloadStream(url, selectedVideoFormat, videoPath);
        const { trimStart, trimEnd } = await inquirer.prompt([
            {
                type: 'input',
                name: 'trimStart',
                message: 'Trim start time (hh:mm:ss, leave blank for start):',
                default: ''
            },
            {
                type: 'input',
                name: 'trimEnd',
                message: 'Trim end time (hh:mm:ss, leave blank for end):',
                default: ''
            }
        ]);
        if (trimStart || trimEnd) {
            const trimmedPath = path.join(__dirname, 'downloads', `${baseName || defaultVideoName.replace(/\.[^.]+$/, '')}-trimmed.${selectedVideoFormat.container}`);
            await new Promise((resolve, reject) => {
                let cmd = ffmpeg(videoPath);
                if (trimStart) cmd = cmd.setStartTime(trimStart);
                if (trimEnd) cmd = cmd.setDuration(calcDuration(trimStart, trimEnd));
                cmd.on('end', () => {
                    fs.unlinkSync(videoPath);
                    fs.renameSync(trimmedPath, videoPath);
                    console.log(`Saved as ${videoPath}`);
                    resolve();
                })
                .on('error', reject)
                .save(trimmedPath);
            });
        } else {
            console.log(`Saved as ${videoPath}`);
        }
    } else if (downloadType === 'both') {
        // Download video and audio separately and merge
        const videoChoices = videoFormats.map(format => ({
            name: `${format.qualityLabel || ''} - ${format.container}`,
            value: format.itag,
            container: format.container
        }));
        // Only show audio formats with compatible containers (mp4)
        const selectedVideoContainer = videoChoices.length ? videoChoices[0].container : 'mp4';
        const compatibleAudioFormats = audioFormats.filter(format => format.container === selectedVideoContainer);
        const audioChoices = compatibleAudioFormats.map(format => ({
            name: `${format.audioBitrate || ''}kbps - ${format.container}`,
            value: format.itag
        }));
        const { videoFormat } = await inquirer.prompt({
            type: 'list',
            name: 'videoFormat',
            message: 'Choose the video resolution:',
            choices: videoChoices
        });
        const selectedVideoFormat = info.formats.find(f => f.itag === videoFormat);
        // Filter audio choices again for selected video container
        const compatibleAudioChoices = audioFormats.filter(format => format.container === selectedVideoFormat.container)
            .map(format => ({
                name: `${format.audioBitrate || ''}kbps - ${format.container}`,
                value: format.itag
            }));
        if (compatibleAudioChoices.length === 0) {
            console.log(`No compatible audio formats found for video container '${selectedVideoFormat.container}'. Please choose a different video format.`);
            return;
        }
        const { audioFormat } = await inquirer.prompt({
            type: 'list',
            name: 'audioFormat',
            message: 'Choose the audio quality:',
            choices: compatibleAudioChoices
        });
        const selectedAudioFormat = info.formats.find(f => f.itag === audioFormat);
        const defaultOutputName = `${sanitizedTitle}-${year}-${month}-${day}-${hours}-${minutes}-${seconds}.mp4`;
        const { customName } = await inquirer.prompt({
            type: 'input',
            name: 'customName',
            message: 'Rename output file (leave blank for default):',
            default: defaultOutputName
        });
        const videoPath = path.join(__dirname, `downloads/${sanitizedTitle}-${year}-${month}-${day}-${hours}-${minutes}-${seconds}-video.${selectedVideoFormat.container}`);
        const audioPath = path.join(__dirname, `downloads/${sanitizedTitle}-${year}-${month}-${day}-${hours}-${minutes}-${seconds}-audio.${selectedAudioFormat.container}`);
        let baseName = customName;
        if (baseName && baseName.toLowerCase().endsWith('.mp4')) {
            baseName = baseName.slice(0, -4);
        }
        const outputPath = path.join(__dirname, 'downloads', `${baseName || defaultOutputName.slice(0, -4)}.mp4`);
        await downloadStream(url, selectedVideoFormat, videoPath);
        await downloadStream(url, selectedAudioFormat, audioPath);
        await mergeFiles(videoPath, audioPath, outputPath);
        const { trimStart, trimEnd } = await inquirer.prompt([
            {
                type: 'input',
                name: 'trimStart',
                message: 'Trim start time (hh:mm:ss, leave blank for start):',
                default: ''
            },
            {
                type: 'input',
                name: 'trimEnd',
                message: 'Trim end time (hh:mm:ss, leave blank for end):',
                default: ''
            }
        ]);
        if (trimStart || trimEnd) {
            const trimmedPath = path.join(__dirname, 'downloads', `${baseName || defaultOutputName.slice(0, -4)}-trimmed.mp4`);
            let duration = calcDuration(trimStart, trimEnd);
            if (trimEnd && (isNaN(duration) || duration <= 0)) {
                console.error('Invalid trim times: end time must be after start time and in hh:mm:ss format.');
                return;
            }
            await new Promise((resolve, reject) => {
                let cmd = ffmpeg(outputPath);
                if (trimStart) cmd = cmd.setStartTime(trimStart);
                if (trimEnd) cmd = cmd.setDuration(duration);
                cmd.on('end', () => {
                    fs.unlinkSync(outputPath);
                    fs.renameSync(trimmedPath, outputPath);
                    console.log(`Saved as ${outputPath}`);
                    resolve();
                })
                .on('error', reject)
                .save(trimmedPath);
            });
        }
    } else if (downloadType === 'av') {
        // Best available single file (video+audio)
        const avChoices = avFormats.map(format => ({
            name: `${format.qualityLabel || ''} - ${format.container}`,
            value: format.itag
        }));
        const { avFormat } = await inquirer.prompt({
            type: 'list',
            name: 'avFormat',
            message: 'Choose the format:',
            choices: avChoices
        });
        const selectedAVFormat = info.formats.find(f => f.itag === avFormat);
        const defaultAVName = `${sanitizedTitle}-${year}-${month}-${day}-${hours}-${minutes}-${seconds}.${selectedAVFormat.container}`;
        const { customName } = await inquirer.prompt({
            type: 'input',
            name: 'customName',
            message: 'Rename output file (leave blank for default):',
            default: defaultAVName
        });
        let baseName = customName;
        if (baseName && baseName.toLowerCase().endsWith(`.${selectedAVFormat.container}`)) {
            baseName = baseName.slice(0, -(selectedAVFormat.container.length + 1));
        }
        const avPath = path.join(__dirname, 'downloads', `${baseName || defaultAVName.replace(/\.[^.]+$/, '')}.${selectedAVFormat.container}`);
        await downloadStream(url, selectedAVFormat, avPath);
        const { trimStart, trimEnd } = await inquirer.prompt([
            {
                type: 'input',
                name: 'trimStart',
                message: 'Trim start time (hh:mm:ss, leave blank for start):',
                default: ''
            },
            {
                type: 'input',
                name: 'trimEnd',
                message: 'Trim end time (hh:mm:ss, leave blank for end):',
                default: ''
            }
        ]);
        if (trimStart || trimEnd) {
            const trimmedPath = path.join(__dirname, 'downloads', `${baseName || defaultAVName.replace(/\.[^.]+$/, '')}-trimmed.${selectedAVFormat.container}`);
            await new Promise((resolve, reject) => {
                let cmd = ffmpeg(avPath);
                if (trimStart) cmd = cmd.setStartTime(trimStart);
                if (trimEnd) cmd = cmd.setDuration(calcDuration(trimStart, trimEnd));
                cmd.on('end', () => {
                    fs.unlinkSync(avPath);
                    fs.renameSync(trimmedPath, avPath);
                    console.log(`Saved as ${avPath}`);
                    resolve();
                })
                .on('error', reject)
                .save(trimmedPath);
            });
        } else {
            console.log(`Saved as ${avPath}`);
        }
// Helper to calculate duration from start/end in hh:mm:ss
function calcDuration(start, end) {
    if (!end) return undefined;
    if (!start) start = '00:00:00';
    const toSeconds = t => {
        const [h, m, s] = t.split(':').map(Number);
        return h * 3600 + m * 60 + s;
    };
    return toSeconds(end) - toSeconds(start);
}
    }
}

main().catch(err => {
    console.error('Error:', err);
});
