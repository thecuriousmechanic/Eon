<?php
// Regenerate the two synthetic JPEGs with PHP Imagick and DejaVu Sans.
// Neither the font nor an external renderer is required to run OCR or the tests.
function render($name, $width, $height, $size, $labels) {
    $image = new Imagick();
    $image->newImage($width, $height, 'white');
    $pen = new ImagickDraw();
    $pen->setFillColor('#142030');
    $pen->setFont('/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf');
    $pen->setFontSize($size);
    foreach ($labels as [$x, $baseline, $text]) {
        $image->annotateImage($pen, $x, $baseline, 0, $text);
    }
    $image->setImageFormat('jpeg');
    $image->setImageCompressionQuality(90);
    $image->stripImage();
    $image->writeImage(__DIR__ . "/$name.jpg");
}

render('settings', 640, 240, 32, [[38, 63, 'Settings'], [393, 60, 'Search'], [87, 190, 'Cancel']]);
render('account', 1280, 720, 40, [[860, 200, 'Profile'], [145, 555, 'Save']]);
