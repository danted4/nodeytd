import ytdl from '@distube/ytdl-core';
import inquirer from 'inquirer';
import fs from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';
import cliProgress from 'cli-progress';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

/**
 * getVideoInfo - gets info about a youtube video
 * @param {string} url - video url
 * @returns {void}
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
 * downloadVideo - handles downloading for selected video with progress indicator
 * @param {string} url - video url
 * @param {*} format - selected format
 * @returns {void}
 */
async function downloadVideo(url, format) {
    try {
        const info = await getVideoInfo(url);
        const formatToDownload = ytdl.chooseFormat(info.formats, { quality: format });

        if (!formatToDownload) {
            console.log('Selected format not available.');
            return;
        }

        const padZero = (num) => num.toString().padStart(2, '0');
        const dt = new Date();
        
        const year = dt.getFullYear();
        const month = padZero(dt.getMonth() + 1);
        const day = padZero(dt.getDate());
        const hours = padZero(dt.getHours());
        const minutes = padZero(dt.getMinutes());
        const seconds = padZero(dt.getSeconds());
        const output = path.join(__dirname, `downloads/${info.videoDetails.title}-${year}-${month}-${day}-${hours}-${minutes}-${seconds}.${formatToDownload.container}`);
        
        console.log(`Downloading to ${output}...`);
        const fileSize = parseInt(formatToDownload.contentLength, 10) || 0;
        
        const progressBar = new cliProgress.SingleBar({
            format: 'Downloading |{bar}| {percentage}% || {value}/{total} MB || {eta}s',
            barCompleteChar: '\u2588',
            barIncompleteChar: '\u2591',
            hideCursor: true,
        }, cliProgress.Presets.shades_classic);

        if (fileSize > 0) {
            progressBar.start(fileSize, 0);
        } else {
            console.log('File size not available! Downloading...');
        }

        const fileStream = fs.createWriteStream(output);
        const videoStream = ytdl(url, { format: formatToDownload });

        videoStream
            .on('data', chunk => {
                if (fileSize > 0) {
                    progressBar.increment(chunk.length);
                }
            })
            .on('end', () => {
                if (fileSize > 0) {
                    progressBar.stop();
                }
                console.log('Download complete!');
            })
            .on('error', error => {
                console.error('Error downloading video:', error);
                if (fileSize > 0) {
                    progressBar.stop();
                }
            })
            .pipe(fileStream);

        fileStream.on('finish', () => {
            console.log('File write complete!');
        });

    } catch (e) {
        console.error(`Error: ${e}`);
    }
}

/**
 * main - main function
 * @returns {void}
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
    
    const formats = info.formats
        .filter(format => format.hasAudio)
        .map(format => ({
            name: format.qualityLabel ? `${format.qualityLabel} - ${format.container}` : `${format.itag} - ${format.container}`,
            value: format.itag
        }));

    if (formats.length === 0) {
        console.log('No downloadable formats with audio found.');
        return;
    }

    const highestQualityAudio = ytdl.chooseFormat(info.formats.filter(f => f.hasAudio && !f.hasVideo), { quality: 'highestaudio' });
    const highestQualityVideo = ytdl.chooseFormat(info.formats.filter(f => f.hasVideo && f.hasAudio), { quality: 'highestvideo' });

    const choices = formats.map(format => ({
        name: format.name,
        value: format.value
    }));

    if (highestQualityAudio) {
        choices.unshift({
            name: `Highest Quality Audio (${highestQualityAudio.audioBitrate}kbps)`,
            value: highestQualityAudio.itag
        });
    }

    if (highestQualityVideo) {
        choices.unshift({
            name: `Highest Quality Video (${highestQualityVideo.qualityLabel})`,
            value: highestQualityVideo.itag
        });
    }

    const resolutionQuestion = {
        type: 'list',
        name: 'format',
        message: 'Choose the resolution quality with audio or select an option:',
        choices: choices
    };
    const { format } = await inquirer.prompt(resolutionQuestion);
    
    await downloadVideo(url, format);
}

main().catch(err => {
    console.error('Error:', err);
});
