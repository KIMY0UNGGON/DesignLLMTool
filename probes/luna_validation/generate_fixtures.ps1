$ErrorActionPreference = 'Stop'
# Windows PowerShell 5.1 reads this BOM-less file in the ANSI code page, so every non-ASCII text is written as a \uXXXX escape.
# Here-strings use @@name@@ tokens that Expand-FixtureText replaces before a part is written.
$fixtureText = @{
    jari = [regex]::Unescape('\uC790\uB9AC\uCC28\uC9C0')
    basic = [regex]::Unescape('\uAE30\uBCF8 \uD14D\uC2A4\uD2B8')
    note = [regex]::Unescape('\uC9C1\uC811 \uC11C\uC2DD \uBCF4\uC874 \uD655\uC778')
    merged = [regex]::Unescape('\uBCD1\uD569 \uD45C \uC140')
    left = [regex]::Unescape('\uC67C\uCABD \uC140')
    right = [regex]::Unescape('\uC624\uB978\uCABD \uC140')
    eoullim = [regex]::Unescape('\uC5B4\uC6B8\uB9BC')
    geulap = [regex]::Unescape('\uAE00\uC55E\uC73C\uB85C')
    geuldwi = [regex]::Unescape('\uAE00\uB4A4\uB85C')
    geulja = [regex]::Unescape('\uAE00\uC790\uCC98\uB7FC \uCDE8\uAE09')
    arrow = [regex]::Unescape('\u2192')
}
function Expand-FixtureText([string]$Text) {
    foreach ($key in $fixtureText.Keys) { $Text = $Text.Replace('@@' + $key + '@@', $fixtureText[$key]) }
    $Text
}
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
Add-Type -AssemblyName System.Drawing

$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$fixtureRoot = Join-Path $repoRoot 'fixtures'
$sourceRoot = Join-Path $fixtureRoot '_sources/rhwp'
$utf8NoBom = [Text.UTF8Encoding]::new($false)
$fixedZipTime = [DateTimeOffset]::Parse('2020-01-01T00:00:00Z')

function Write-Utf8NoBom([string]$Path, [string]$Text) {
    [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($Path)) | Out-Null
    [IO.File]::WriteAllText($Path, $Text, $utf8NoBom)
}

function Add-ZipBytes($Archive, [string]$Path, [byte[]]$Bytes, $Compression = [IO.Compression.CompressionLevel]::Optimal) {
    $entry = $Archive.CreateEntry($Path, $Compression)
    $entry.LastWriteTime = $fixedZipTime
    $stream = $entry.Open()
    try { $stream.Write($Bytes, 0, $Bytes.Length) } finally { $stream.Dispose() }
}

function Add-ZipText($Archive, [string]$Path, [string]$Text) {
    Add-ZipBytes $Archive $Path $utf8NoBom.GetBytes((Expand-FixtureText $Text))
}

function New-ZipPackage([string]$Path, $Parts) {
    [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($Path)) | Out-Null
    $file = [IO.File]::Open($Path, [IO.FileMode]::Create, [IO.FileAccess]::ReadWrite)
    $archive = [IO.Compression.ZipArchive]::new($file, [IO.Compression.ZipArchiveMode]::Create, $false)
    try {
        foreach ($part in $Parts) {
            if ($part.Bytes) { Add-ZipBytes $archive $part.Path $part.Bytes }
            else { Add-ZipText $archive $part.Path $part.Text }
        }
    } finally { $archive.Dispose() }
}

function Get-Sha256([string]$Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-TextSha256([string]$Text) {
    $sha = [Security.Cryptography.SHA256]::Create()
    try { ([BitConverter]::ToString($sha.ComputeHash($utf8NoBom.GetBytes($Text)))).Replace('-', '').ToLowerInvariant() }
    finally { $sha.Dispose() }
}

function Get-ZipEntrySha256([string]$Path, [string]$EntryPath) {
    $archive = [IO.Compression.ZipFile]::OpenRead($Path)
    try {
        $entry = $archive.GetEntry($EntryPath)
        if (!$entry) { throw "Missing ZIP part $EntryPath in $Path" }
        $stream = $entry.Open()
        $sha = [Security.Cryptography.SHA256]::Create()
        try { ([BitConverter]::ToString($sha.ComputeHash($stream))).Replace('-', '').ToLowerInvariant() }
        finally { $sha.Dispose(); $stream.Dispose() }
    } finally { $archive.Dispose() }
}

$directories = @('assets', 'office', 'hwpx', 'hwp', 'data', 'pdf')
foreach ($directory in $directories) {
    [IO.Directory]::CreateDirectory((Join-Path $fixtureRoot $directory)) | Out-Null
}

# A locally authored checker PNG. Keep the previous bad CRC bytes only as a
# negative decoder fixture; never embed that malformed image in a document.
$bitmap = [Drawing.Bitmap]::new(16, 12, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
$palette = @(
    [Drawing.Color]::FromArgb(255, 220, 40, 40)
    [Drawing.Color]::FromArgb(255, 40, 150, 70)
    [Drawing.Color]::FromArgb(255, 40, 90, 220)
    [Drawing.Color]::FromArgb(255, 245, 190, 40)
)
for ($y = 0; $y -lt 12; $y++) {
    for ($x = 0; $x -lt 16; $x++) {
        $cell = (([int][Math]::Floor($x / 4)) + ([int][Math]::Floor($y / 3))) % $palette.Count
        $bitmap.SetPixel($x, $y, $palette[$cell])
    }
}
$pngStream = [IO.MemoryStream]::new()
try { $bitmap.Save($pngStream, [Drawing.Imaging.ImageFormat]::Png); $pixelPng = $pngStream.ToArray() }
finally { $pngStream.Dispose(); $bitmap.Dispose() }
$invalidPng = [Convert]::FromBase64String('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO8K6iQAAAAASUVORK5CYII=')
$pixelPath = Join-Path $fixtureRoot 'assets/pixel.png'
[IO.File]::WriteAllBytes($pixelPath, $pixelPng)
$invalidPngPath = Join-Path $fixtureRoot 'assets/invalid-crc.png'
[IO.File]::WriteAllBytes($invalidPngPath, $invalidPng)
$pixelHash = Get-Sha256 $pixelPath
$pptxOpaqueXml = '<?xml version="1.0" encoding="UTF-8"?><fixture:opaque xmlns:fixture="urn:designllm:luna-fixture"><fixture:sentinel>Preserve-XML-PPTX-42</fixture:sentinel></fixture:opaque>'
$docxOpaqueXml = '<?xml version="1.0" encoding="UTF-8"?><fixture:opaque xmlns:fixture="urn:designllm:luna-fixture"><fixture:sentinel>Preserve-XML-DOCX-42</fixture:sentinel></fixture:opaque>'
$hwpxOpaqueXml = '<?xml version="1.0" encoding="UTF-8"?><fixture:opaque xmlns:fixture="urn:designllm:luna-fixture"><fixture:sentinel>Preserve-XML-HWPX-42</fixture:sentinel></fixture:opaque>'
$pptxCnvPrRegion = '<a:extLst><a:ext uri="{FF2B5EF4-FFF2-40B4-BE49-F238E27FC236}"><a16:creationId xmlns:a16="http://schemas.microsoft.com/office/drawing/2014/main" id="{6C1F2A10-1B2C-4D3E-8F40-5A6B7C8D9E01}"/></a:ext></a:extLst>'
$pptxAlternateRegion = '<mc:AlternateContent><mc:Choice Requires="p14"><p:sp><p:nvSpPr><p:cNvPr id="4" name="Alternate ellipsis"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="5029200" y="1600200"/><a:ext cx="685800" cy="685800"/></a:xfrm><a:prstGeom prst="ellipse"><a:avLst/></a:prstGeom><a:solidFill><a:srgbClr val="ED7D31"/></a:solidFill></p:spPr></p:sp></mc:Choice><mc:Fallback><p:sp><p:nvSpPr><p:cNvPr id="5" name="Fallback ellipsis"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="5029200" y="1600200"/><a:ext cx="685800" cy="685800"/></a:xfrm><a:prstGeom prst="ellipse"><a:avLst/></a:prstGeom><a:solidFill><a:srgbClr val="ED7D31"/></a:solidFill></p:spPr></p:sp></mc:Fallback></mc:AlternateContent>'
$docxAlternateRegion = '<mc:AlternateContent><mc:Choice Requires="w14"><w:r><w:rPr><w14:textOutline w14:w="9525"><w14:solidFill><w14:srgbClr w14:val="000000"/></w14:solidFill></w14:textOutline></w:rPr><w:t>Outlined sibling payload</w:t></w:r></mc:Choice><mc:Fallback><w:r><w:t>Fallback sibling payload</w:t></w:r></mc:Fallback></mc:AlternateContent>'
$hwpxTextRegion = '<hp:t xmlns:hx="urn:designllm:luna-fixture" hx:mark="preserve-text-42">' + $fixtureText.jari + '</hp:t>'
$pptxOpaqueHash = Get-TextSha256 $pptxOpaqueXml
$docxOpaqueHash = Get-TextSha256 $docxOpaqueXml
$hwpxOpaqueHash = Get-TextSha256 $hwpxOpaqueXml
$pptxCnvPrHash = Get-TextSha256 $pptxCnvPrRegion
$pptxAlternateHash = Get-TextSha256 $pptxAlternateRegion
$docxAlternateHash = Get-TextSha256 $docxAlternateRegion
$hwpxTextRegionHash = Get-TextSha256 $hwpxTextRegion

$pptxParts = @(
    @{ Path = '[Content_Types].xml'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/><Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/><Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/><Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/><Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/><Override PartName="/ppt/notesMasters/notesMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.notesMaster+xml"/><Override PartName="/ppt/notesSlides/notesSlide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml"/><Override PartName="/ppt/luna/opaque.xml" ContentType="application/xml"/><Override PartName="/ppt/presProps.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presProps+xml"/><Override PartName="/ppt/viewProps.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.viewProps+xml"/><Override PartName="/ppt/tableStyles.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.tableStyles+xml"/><Override PartName="/ppt/theme/theme2.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/><Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/><Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/></Types>
'@ }
    @{ Path = '_rels/.rels'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/></Relationships>
'@ }
    @{ Path = 'ppt/presentation.xml'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId2"/></p:sldMasterIdLst><p:notesMasterIdLst><p:notesMasterId r:id="rId3"/></p:notesMasterIdLst><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst><p:sldSz cx="9144000" cy="5143500" type="screen16x9"/><p:notesSz cx="6858000" cy="9144000"/><p:defaultTextStyle/></p:presentation>
'@ }
    @{ Path = 'ppt/_rels/presentation.xml.rels'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesMaster" Target="notesMasters/notesMaster1.xml"/><Relationship Id="rIdOpaque" Type="urn:designllm:fixture:opaque" Target="luna/opaque.xml"/><Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/presProps" Target="presProps.xml"/><Relationship Id="rId5" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/viewProps" Target="viewProps.xml"/><Relationship Id="rId6" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/tableStyles" Target="tableStyles.xml"/><Relationship Id="rId7" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="theme/theme1.xml"/></Relationships>
'@ }
    @{ Path = 'ppt/slides/slide1.xml'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:p14="http://schemas.microsoft.com/office/powerpoint/2010/main" xmlns:custom="urn:designllm:luna-fixture" mc:Ignorable="custom"><p:cSld name="Luna fixture"><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr><p:sp><p:nvSpPr><p:cNvPr id="2" name="Editable fixture label"><a:extLst><a:ext uri="{FF2B5EF4-FFF2-40B4-BE49-F238E27FC236}"><a16:creationId xmlns:a16="http://schemas.microsoft.com/office/drawing/2014/main" id="{6C1F2A10-1B2C-4D3E-8F40-5A6B7C8D9E01}"/></a:ext></a:extLst></p:cNvPr><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr custom:flag="keep"><a:xfrm><a:off x="914400" y="914400"/><a:ext cx="3657600" cy="685800"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:solidFill><a:srgbClr val="E6F0FA"/></a:solidFill><a:ln><a:solidFill><a:srgbClr val="315A7D"/></a:solidFill><a:prstDash val="solid"/></a:ln></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="ko-KR" sz="1800" b="1"><a:solidFill><a:srgbClr val="17365D"/></a:solidFill><a:latin typeface="Arial"/><a:ea typeface="Malgun Gothic"/></a:rPr><a:t>@@basic@@</a:t></a:r><a:endParaRPr lang="ko-KR"/></a:p></p:txBody></p:sp><p:pic><p:nvPicPr><p:cNvPr id="3" name="Generated checker preservation image" descr="16 by 12 checker fixture sentinel"/><p:cNvPicPr><a:picLocks noChangeAspect="1"/></p:cNvPicPr><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="rId1"/><a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr><a:xfrm><a:off x="4572000" y="914400"/><a:ext cx="457200" cy="342900"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr></p:pic>__LUNA_PPTX_ALT_REGION__</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>
'@ }
    @{ Path = 'ppt/slides/_rels/slide1.xml.rels'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/pixel.png"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide1.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/></Relationships>
'@ }
    @{ Path = 'ppt/slideMasters/slideMaster1.xml'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld name="Fixture master"><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst><p:txStyles><p:titleStyle><a:lvl1pPr><a:defRPr sz="2800"/></a:lvl1pPr></p:titleStyle><p:bodyStyle><a:lvl1pPr><a:defRPr sz="1800"/></a:lvl1pPr></p:bodyStyle><p:otherStyle><a:defPPr><a:defRPr sz="1800"/></a:defPPr></p:otherStyle></p:txStyles></p:sldMaster>
'@ }
    @{ Path = 'ppt/slideMasters/_rels/slideMaster1.xml.rels'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/></Relationships>
'@ }
    @{ Path = 'ppt/slideLayouts/slideLayout1.xml'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="blank" preserve="1"><p:cSld name="Fixture blank layout"><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>
'@ }
    @{ Path = 'ppt/slideLayouts/_rels/slideLayout1.xml.rels'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/></Relationships>
'@ }
    @{ Path = 'ppt/theme/theme1.xml'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Luna fixture theme"><a:themeElements><a:clrScheme name="Luna fixture"><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="17365D"/></a:dk2><a:lt2><a:srgbClr val="E6F0FA"/></a:lt2><a:accent1><a:srgbClr val="315A7D"/></a:accent1><a:accent2><a:srgbClr val="70AD47"/></a:accent2><a:accent3><a:srgbClr val="ED7D31"/></a:accent3><a:accent4><a:srgbClr val="A5A5A5"/></a:accent4><a:accent5><a:srgbClr val="FFC000"/></a:accent5><a:accent6><a:srgbClr val="5B9BD5"/></a:accent6><a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme><a:fontScheme name="Luna fixture fonts"><a:majorFont><a:latin typeface="Arial"/><a:ea typeface="Malgun Gothic"/><a:cs typeface="Arial"/></a:majorFont><a:minorFont><a:latin typeface="Arial"/><a:ea typeface="Malgun Gothic"/><a:cs typeface="Arial"/></a:minorFont></a:fontScheme><a:fmtScheme name="Luna fixture format"><a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:gradFill rotWithShape="1"><a:gsLst><a:gs pos="0"><a:schemeClr val="phClr"/></a:gs><a:gs pos="100000"><a:schemeClr val="phClr"/></a:gs></a:gsLst><a:lin ang="0" scaled="1"/></a:gradFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst><a:lnStyleLst><a:ln w="6350"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:prstDash val="solid"/></a:ln><a:ln w="12700"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:prstDash val="solid"/></a:ln><a:ln w="19050"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:prstDash val="solid"/></a:ln></a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:bgFillStyleLst></a:fmtScheme></a:themeElements><a:objectDefaults/><a:extraClrSchemeLst/></a:theme>
'@ }
    @{ Path = 'ppt/notesMasters/notesMaster1.xml'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:notesMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld name="Fixture notes master"><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:notesStyle><a:lvl1pPr><a:defRPr sz="1200"/></a:lvl1pPr></p:notesStyle></p:notesMaster>
'@ }
    @{ Path = 'ppt/notesMasters/_rels/notesMaster1.xml.rels'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme2.xml"/></Relationships>
'@ }
    @{ Path = 'ppt/notesSlides/notesSlide1.xml'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:notes xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld name="Fixture notes"><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr><p:sp><p:nvSpPr><p:cNvPr id="2" name="Notes placeholder"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US"/><a:t>Preserve note sentinel Luna-Note-42.</a:t></a:r><a:endParaRPr lang="en-US"/></a:p></p:txBody></p:sp></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:notes>
'@ }
    @{ Path = 'ppt/notesSlides/_rels/notesSlide1.xml.rels'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="../slides/slide1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesMaster" Target="../notesMasters/notesMaster1.xml"/></Relationships>
'@ }
    @{ Path = 'ppt/presProps.xml'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentationPr xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"/>
'@ }
    @{ Path = 'ppt/viewProps.xml'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:viewPr xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"/>
'@ }
    @{ Path = 'ppt/tableStyles.xml'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:tblStyleLst xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" def="{5C22544A-7EE6-4342-B048-85BDC9FD1C3A}"/>
'@ }
    @{ Path = 'docProps/core.xml'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"><dc:title>Basic preserve fixture</dc:title><dcterms:created xsi:type="dcterms:W3CDTF">2020-01-01T00:00:00Z</dcterms:created><dcterms:modified xsi:type="dcterms:W3CDTF">2020-01-01T00:00:00Z</dcterms:modified></cp:coreProperties>
'@ }
    @{ Path = 'docProps/app.xml'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties" xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes"><Application>DesignLLMTool fixture generator</Application><Slides>1</Slides><Notes>1</Notes></Properties>
'@ }
    @{ Path = 'ppt/luna/opaque.xml'; Text = $pptxOpaqueXml }
    @{ Path = 'ppt/media/pixel.png'; Bytes = $pixelPng }
)
# PowerPoint needs a theme part of its own for the notes master: a copy of the slide master theme under another name.
$pptxTheme1 = $pptxParts | Where-Object { $_.Path -eq 'ppt/theme/theme1.xml' }
$pptxParts += @{ Path = 'ppt/theme/theme2.xml'; Text = $pptxTheme1.Text.Replace('name="Luna fixture theme"', 'name="Luna fixture notes theme"') }
$pptxSlidePart = $pptxParts | Where-Object { $_.Path -eq 'ppt/slides/slide1.xml' }
if (!$pptxSlidePart.Text.Contains('__LUNA_PPTX_ALT_REGION__')) { throw 'PPTX opaque-region insertion anchor is missing.' }
$pptxSlidePart.Text = $pptxSlidePart.Text.Replace('__LUNA_PPTX_ALT_REGION__', $pptxAlternateRegion)
New-ZipPackage (Join-Path $fixtureRoot 'office/basic-preserve.pptx') $pptxParts

$docxParts = @(
    @{ Path = '[Content_Types].xml'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/><Override PartName="/word/header1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml"/><Override PartName="/word/footer1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml"/><Override PartName="/word/settings.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml"/><Override PartName="/customXml/itemProps1.xml" ContentType="application/vnd.openxmlformats-officedocument.customXmlProperties+xml"/></Types>
'@ }
    @{ Path = '_rels/.rels'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>
'@ }
    @{ Path = 'word/document.xml'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture"><w:body><w:p><w:pPr><w:pStyle w:val="LunaTitle"/></w:pPr><w:r><w:rPr><w:b/><w:color w:val="17365D"/><w:sz w:val="32"/></w:rPr><w:t>@@basic@@</w:t></w:r></w:p><w:p><w:r><w:rPr><w:rStyle w:val="LunaAccent"/></w:rPr><w:t>@@note@@</w:t></w:r></w:p><w:tbl><w:tblPr><w:tblW w:w="0" w:type="auto"/><w:tblBorders><w:top w:val="single" w:sz="4" w:color="315A7D"/><w:left w:val="single" w:sz="4" w:color="315A7D"/><w:bottom w:val="single" w:sz="4" w:color="315A7D"/><w:right w:val="single" w:sz="4" w:color="315A7D"/><w:insideH w:val="single" w:sz="4" w:color="A6A6A6"/><w:insideV w:val="single" w:sz="4" w:color="A6A6A6"/></w:tblBorders></w:tblPr><w:tblGrid><w:gridCol w:w="2400"/><w:gridCol w:w="2400"/></w:tblGrid><w:tr><w:tc><w:tcPr><w:gridSpan w:val="2"/></w:tcPr><w:p><w:r><w:t>@@merged@@</w:t></w:r></w:p></w:tc></w:tr><w:tr><w:tc><w:p><w:r><w:t>@@left@@</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>@@right@@</w:t></w:r></w:p></w:tc></w:tr></w:tbl><w:p><w:r><w:drawing><wp:inline distT="0" distB="0" distL="0" distR="0"><wp:extent cx="152400" cy="114300"/><wp:docPr id="1" name="Generated checker fixture" descr="16 by 12 checker preservation sentinel"/><wp:cNvGraphicFramePr><a:graphicFrameLocks noChangeAspect="1"/></wp:cNvGraphicFramePr><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture"><pic:pic><pic:nvPicPr><pic:cNvPr id="0" name="pixel.png"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed="rId2"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill><pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="152400" cy="114300"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p><w:sectPr><w:headerReference w:type="default" r:id="rId3"/><w:footerReference w:type="default" r:id="rId4"/><w:pgSz w:w="11906" w:h="16838"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="708" w:footer="708" w:gutter="0"/></w:sectPr></w:body></w:document>
'@ }
    @{ Path = 'word/_rels/document.xml.rels'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/pixel.png"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/><Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer" Target="footer1.xml"/><Relationship Id="rId5" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXml" Target="../customXml/item1.xml"/><Relationship Id="rId6" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings" Target="settings.xml"/></Relationships>
'@ }
    @{ Path = 'word/styles.xml'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Arial" w:hAnsi="Arial" w:eastAsia="Malgun Gothic"/><w:lang w:val="en-US" w:eastAsia="ko-KR"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr/></w:pPrDefault></w:docDefaults><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:qFormat/></w:style><w:style w:type="paragraph" w:styleId="LunaTitle"><w:name w:val="Luna Fixture Title"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:spacing w:after="120"/></w:pPr><w:rPr><w:b/><w:color w:val="17365D"/><w:sz w:val="32"/></w:rPr></w:style><w:style w:type="character" w:styleId="LunaAccent"><w:name w:val="Luna Accent"/><w:rPr><w:rFonts w:ascii="Arial" w:eastAsia="Malgun Gothic"/><w:i/><w:color w:val="0066CC"/></w:rPr></w:style></w:styles>
'@ }
    @{ Path = 'word/header1.xml'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:hdr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:p><w:r><w:t>Fixture header</w:t></w:r></w:p></w:hdr>
'@ }
    @{ Path = 'word/footer1.xml'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:ftr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:p><w:r><w:t>Fixture footer</w:t></w:r></w:p></w:ftr>
'@ }
    @{ Path = 'word/settings.xml'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:settings xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:defaultTabStop w:val="720"/></w:settings>
'@ }
    @{ Path = 'customXml/item1.xml'; Text = $docxOpaqueXml }
    @{ Path = 'customXml/itemProps1.xml'; Text = @'
<?xml version="1.0" encoding="UTF-8"?><ds:datastoreItem xmlns:ds="http://schemas.openxmlformats.org/officeDocument/2006/customXml" ds:itemID="{1A12C550-65A4-4CBF-A2EF-6B8D048BEA1F}"/>
'@ }
    @{ Path = 'customXml/_rels/item1.xml.rels'; Text = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXmlProps" Target="itemProps1.xml"/></Relationships>
'@ }
    @{ Path = 'word/media/pixel.png'; Bytes = $pixelPng }
)
$docxDocumentPart = $docxParts | Where-Object { $_.Path -eq 'word/document.xml' }
$docxDocumentXml = $docxDocumentPart.Text
$docxDocumentXml = $docxDocumentXml.Replace(
    'xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture">',
    'xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml">'
)
$docxOpen = '<w:p><w:pPr><w:pStyle w:val="LunaTitle"/></w:pPr>'
$docxTargetOpen = '<w:p w14:paraId="0A000001"><w:pPr><w:pStyle w:val="LunaTitle"/></w:pPr>'
$docxClose = '<w:t>@@basic@@</w:t></w:r></w:p>'   # raw here-string text: the token is expanded when the part is written
$docxTargetClose = '<w:t>@@basic@@</w:t></w:r>__LUNA_DOCX_ALT_REGION__</w:p>'
if (!$docxDocumentXml.Contains($docxOpen) -or !$docxDocumentXml.Contains($docxClose)) { throw 'DOCX opaque-region insertion anchor is missing.' }
$docxDocumentXml = $docxDocumentXml.Replace($docxOpen, $docxTargetOpen).Replace($docxClose, $docxTargetClose)
$docxDocumentPart.Text = $docxDocumentXml.Replace('__LUNA_DOCX_ALT_REGION__', $docxAlternateRegion)
New-ZipPackage (Join-Path $fixtureRoot 'office/basic-preserve.docx') $docxParts

# Derive one HWPX preservation case from a genuine MIT-licensed HWPX sample.
$hwpxSource = Join-Path $sourceRoot 'test-image.hwpx'
$hwpxTarget = Join-Path $fixtureRoot 'hwpx/opaque-image-preserve.hwpx'
if (!(Test-Path -LiteralPath $hwpxSource)) { throw "Missing pinned source fixture: $hwpxSource" }
$sourceArchive = [IO.Compression.ZipFile]::OpenRead($hwpxSource)
$hwpxBuilt = Join-Path ([IO.Path]::GetTempPath()) ('luna-hwpx-' + [guid]::NewGuid().ToString('N') + '.hwpx')
$targetFile = [IO.File]::Open($hwpxBuilt, [IO.FileMode]::Create, [IO.FileAccess]::ReadWrite)
$targetArchive = [IO.Compression.ZipArchive]::new($targetFile, [IO.Compression.ZipArchiveMode]::Create, $false)
try {
    foreach ($entry in $sourceArchive.Entries) {
        if ($entry.FullName -eq 'Contents/section0.xml') {
            $reader = [IO.StreamReader]::new($entry.Open(), [Text.Encoding]::UTF8)
            try { $sectionXml = $reader.ReadToEnd() } finally { $reader.Dispose() }
            $textAnchor = '<hp:t>' + $fixtureText.jari + '</hp:t>'
            if ([regex]::Matches($sectionXml, [regex]::Escape($textAnchor)).Count -ne 1) { throw 'HWPX text preservation anchor must occur exactly once.' }
            $sectionXml = $sectionXml.Replace($textAnchor, $hwpxTextRegion)
            Add-ZipText $targetArchive $entry.FullName $sectionXml
        } elseif ($entry.FullName -eq 'Contents/content.hpf') {
            $reader = [IO.StreamReader]::new($entry.Open(), [Text.Encoding]::UTF8)
            try { $contentHpf = $reader.ReadToEnd() } finally { $reader.Dispose() }
            if (!$contentHpf.Contains('</opf:manifest>')) { throw 'HWPX source has no OPF manifest close tag.' }
            $contentHpf = $contentHpf.Replace('</opf:manifest>', '<opf:item id="lunaOpaque" href="Luna/opaque.xml" media-type="application/xml"/></opf:manifest>')
            Add-ZipText $targetArchive $entry.FullName $contentHpf
        } else {
            $compression = [IO.Compression.CompressionLevel]::Optimal
            if ($entry.FullName -eq 'mimetype') { $compression = [IO.Compression.CompressionLevel]::NoCompression }
            $copy = $targetArchive.CreateEntry($entry.FullName, $compression)
            $copy.LastWriteTime = $fixedZipTime
            $inputStream = $entry.Open()
            $outputStream = $copy.Open()
            try { $inputStream.CopyTo($outputStream) } finally { $inputStream.Dispose(); $outputStream.Dispose() }
        }
    }
    Add-ZipText $targetArchive 'Luna/opaque.xml' $hwpxOpaqueXml
} finally { $targetArchive.Dispose(); $sourceArchive.Dispose() }
# ZipArchive under Windows PowerShell 5.1 cannot store an entry uncompressed, so the mimetype entry comes out deflated,
# and the HWPX reader refuses that (HWPX_MIMETYPE_INVALID). The rebuilt parts must equal the committed package. The
# committed container is kept unless the rebuilt copy stores its mimetype entry (local header method 0).
function Get-MimetypeMethod([string]$Path) {
    $bytes = [IO.File]::ReadAllBytes($Path)
    if ([BitConverter]::ToUInt16($bytes, 26) -ne 8 -or [Text.Encoding]::ASCII.GetString($bytes, 30, 8) -ne 'mimetype') { return -1 }
    [int][BitConverter]::ToUInt16($bytes, 8)
}
$rebuiltZip = [IO.Compression.ZipFile]::OpenRead($hwpxBuilt)
try { $rebuiltNames = @($rebuiltZip.Entries | ForEach-Object { $_.FullName } | Sort-Object) } finally { $rebuiltZip.Dispose() }
$committedZip = [IO.Compression.ZipFile]::OpenRead($hwpxTarget)
try { $committedNames = @($committedZip.Entries | ForEach-Object { $_.FullName } | Sort-Object) } finally { $committedZip.Dispose() }
if (($rebuiltNames -join '|') -ne ($committedNames -join '|')) { throw 'HWPX rebuilt entry list differs from the committed package.' }
foreach ($name in $rebuiltNames) {
    if ((Get-ZipEntrySha256 $hwpxBuilt $name) -ne (Get-ZipEntrySha256 $hwpxTarget $name)) { throw "HWPX part $name differs from the committed package." }
}
if ((Get-MimetypeMethod $hwpxBuilt) -eq 0) {
    Move-Item -LiteralPath $hwpxBuilt -Destination $hwpxTarget -Force
    Write-Host 'HWPX: rebuilt container written (mimetype stored).'
} else {
    Remove-Item -LiteralPath $hwpxBuilt -Force
    Write-Host 'HWPX: rebuilt parts equal the committed package; the committed container is kept (mimetype is not stored by this PowerShell).'
}
Copy-Item -LiteralPath (Join-Path $sourceRoot 'test-image.hwp') -Destination (Join-Path $fixtureRoot 'hwp/test-image.hwp') -Force

$primaryCsv = @'
condition,mean,sd,n,unit
baseline,0.10,0.01,3,ratio
treated,0.20,0.02,3,ratio
recovery,0.30,0.03,3,ratio
'@
Write-Utf8NoBom (Join-Path $fixtureRoot 'data/known-sd.csv') $primaryCsv

$invalidCsv = @'
case,category,value,error,n,unit,error_type,lower,upper,ci_level
missing_value,Missing,,0.02,3,ratio,SD,,,
ambiguous_thousands,Ambiguous,"1,234",0.01,3,ratio,SD,,,
negative_error,Negative,0.20,-0.01,3,ratio,SD,,,
asymmetric_error,Asymmetric,0.20,,3,ratio,ASYMMETRIC,0.18,0.23,
invalid_bounds,Bounds,0.20,,3,ratio,RANGE,0.25,0.15,
ci_without_level,CI,0.20,,3,ratio,CI,,,
missing_n,NoN,0.20,0.01,,ratio,SD,,,
log_nonpositive_lower,Log,0.10,0.15,3,ratio,SD,,,
duplicate_category,Duplicate,0.10,0.01,3,ratio,SD,,,
duplicate_category,Duplicate,0.20,0.02,3,ratio,SD,,,
'@
Write-Utf8NoBom (Join-Path $fixtureRoot 'data/invalid-cases.csv') $invalidCsv

function Write-PdfAscii($Stream, [string]$Text) {
    $bytes = [Text.Encoding]::ASCII.GetBytes($Text)
    $Stream.Write($bytes, 0, $bytes.Length)
}

$pdfStreams = New-Object 'System.Collections.Generic.List[string]'
for ($page = 1; $page -le 11; $page++) {
    $pdfStreams.Add("BT /F1 10 Tf 1 0 0 1 90 742 Tm (Page $page fixture.) Tj ET`n")
}
$pdfStreams.Add("BT /F1 20 Tf 63.3868057925789 Tz 0.44 Ts 1 0 0 1.081081081081081 90 684 Tm (Evidence: A causes B in this controlled fixture.) Tj ET`nBT /F1 10 Tf 1 0 0 1 90 672 Tm (Printed label: 9.) Tj ET`n")
$pdfObjects = New-Object 'System.Collections.Generic.List[string]'
$pdfObjects.Add('<< /Type /Catalog /Pages 2 0 R /PageLabels << /Nums [0 << /S /D /St 1 >> 11 << /S /D /St 9 >>] >> >>')
$kids = (3..14 | ForEach-Object { "$_ 0 R" }) -join ' '
$pdfObjects.Add("<< /Type /Pages /Kids [ $kids ] /Count 12 >>")
for ($page = 1; $page -le 12; $page++) {
    $contentObject = 14 + $page
    $pdfObjects.Add("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /CropBox [18 20 594 760] /Rotate 0 /UserUnit 1 /Resources << /Font << /F1 27 0 R >> >> /Contents $contentObject 0 R >>")
}
for ($index = 0; $index -lt 12; $index++) {
    $content = $pdfStreams[$index]
    $length = [Text.Encoding]::ASCII.GetByteCount($content)
    $pdfObjects.Add("<< /Length $length >>`nstream`n$content" + 'endstream')
}
$pdfObjects.Add('<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>')
$pdfStream = [IO.MemoryStream]::new()
$offsets = New-Object 'System.Collections.Generic.List[long]'
$offsets.Add(0)
Write-PdfAscii $pdfStream "%PDF-1.7`n"
for ($index = 0; $index -lt $pdfObjects.Count; $index++) {
    $offsets.Add($pdfStream.Position)
    Write-PdfAscii $pdfStream ("{0} 0 obj`n{1}`nendobj`n" -f ($index + 1), $pdfObjects[$index])
}
$xrefOffset = $pdfStream.Position
Write-PdfAscii $pdfStream ("xref`n0 {0}`n0000000000 65535 f `n" -f ($pdfObjects.Count + 1))
for ($index = 1; $index -lt $offsets.Count; $index++) {
    Write-PdfAscii $pdfStream ("{0:0000000000} 00000 n `n" -f $offsets[$index])
}
Write-PdfAscii $pdfStream ("trailer`n<< /Size {0} /Root 1 0 R >>`nstartxref`n{1}`n%%EOF`n" -f ($pdfObjects.Count + 1), $xrefOffset)
$pdfPath = Join-Path $fixtureRoot 'pdf/evidence-p12.pdf'
[IO.File]::WriteAllBytes($pdfPath, $pdfStream.ToArray())
$pdfStream.Dispose()
$pdfUserUnit2Path = Join-Path $fixtureRoot 'pdf/evidence-p12-userunit2.pdf'
$pdfUserUnit2Text = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($pdfPath))
if ([regex]::Matches($pdfUserUnit2Text, '/UserUnit 1').Count -ne 12) { throw 'PDF fixture must declare UserUnit=1 on all 12 pages before derivation.' }
$pdfUserUnit2Text = $pdfUserUnit2Text.Replace('/UserUnit 1', '/UserUnit 2')
[IO.File]::WriteAllBytes($pdfUserUnit2Path, [Text.Encoding]::ASCII.GetBytes($pdfUserUnit2Text))

$commonSources = @{
    generated = 'Directly authored for this test suite; CC0-1.0.'
    rhwp = @{
        repository = 'https://github.com/edwardkim/rhwp'
        commit = 'bc38ff55a7e8acb65aebebe237dca0542480d381'
        license = 'MIT; see fixtures/_sources/rhwp/LICENSE'
    }
}
$hwpxImageHash = Get-ZipEntrySha256 $hwpxSource 'BinData/image1.bmp'
$pptxNotesHash = Get-ZipEntrySha256 (Join-Path $fixtureRoot 'office/basic-preserve.pptx') 'ppt/notesSlides/notesSlide1.xml'
$pptxPresentationRelsHash = Get-ZipEntrySha256 (Join-Path $fixtureRoot 'office/basic-preserve.pptx') 'ppt/_rels/presentation.xml.rels'
$pptxSlideRelsHash = Get-ZipEntrySha256 (Join-Path $fixtureRoot 'office/basic-preserve.pptx') 'ppt/slides/_rels/slide1.xml.rels'
$docxHeaderHash = Get-ZipEntrySha256 (Join-Path $fixtureRoot 'office/basic-preserve.docx') 'word/header1.xml'
$docxFooterHash = Get-ZipEntrySha256 (Join-Path $fixtureRoot 'office/basic-preserve.docx') 'word/footer1.xml'
$docxRelsHash = Get-ZipEntrySha256 (Join-Path $fixtureRoot 'office/basic-preserve.docx') 'word/_rels/document.xml.rels'
$hwpxManifestHash = Get-ZipEntrySha256 $hwpxTarget 'Contents/content.hpf'
$hwpPath = Join-Path $fixtureRoot 'hwp/test-image.hwp'
$hwpSha = Get-Sha256 $hwpPath
$fixtures = @(
    [ordered]@{
        id = 'pptx.basic-preserve'; path = 'office/basic-preserve.pptx'; format = 'pptx'; origin = 'author-created'; license = $commonSources.generated
        sha256 = Get-Sha256 (Join-Path $fixtureRoot 'office/basic-preserve.pptx')
        checks = @(
            [ordered]@{ id = 'native-image-payload'; part = 'ppt/media/pixel.png'; sha256 = $pixelHash }
            [ordered]@{ id = 'opaque-xml-payload'; part = 'ppt/luna/opaque.xml'; sha256 = $pptxOpaqueHash; contains = 'Preserve-XML-PPTX-42' }
            [ordered]@{ id = 'opaque-part-relationship'; part = 'ppt/_rels/presentation.xml.rels'; sha256 = $pptxPresentationRelsHash; contains = 'urn:designllm:fixture:opaque' }
            [ordered]@{ id = 'image-and-notes-relationships'; part = 'ppt/slides/_rels/slide1.xml.rels'; sha256 = $pptxSlideRelsHash }
            [ordered]@{ id = 'notes-payload'; part = 'ppt/notesSlides/notesSlide1.xml'; sha256 = $pptxNotesHash; contains = 'Luna-Note-42' }
            [ordered]@{ id = 'protected-cnvpr-extension'; part = 'ppt/slides/slide1.xml'; anchor = '/p:sld/p:cSld/p:spTree/p:sp/p:nvSpPr/p:cNvPr[@id="2"]/a:extLst'; occurrence = 1; match_count = 1; fragment_sha256 = $pptxCnvPrHash; fragment_xml = $pptxCnvPrRegion }
            [ordered]@{ id = 'protected-alternate-content'; part = 'ppt/slides/slide1.xml'; anchor = '/p:sld/p:cSld/p:spTree/mc:AlternateContent'; occurrence = 1; match_count = 1; fragment_sha256 = $pptxAlternateHash; fragment_xml = $pptxAlternateRegion }
            [ordered]@{ id = 'protected-foreign-attribute'; part = 'ppt/slides/slide1.xml'; anchor = '/p:sld/p:cSld/p:spTree/p:sp/p:spPr/@custom:flag'; occurrence = 1; match_count = 1; fragment_sha256 = Get-TextSha256 'custom:flag="keep"'; fragment_xml = 'custom:flag="keep"' }
        )
        expected = [ordered]@{
            editable_text = $fixtureText.basic; shape_id = 2; font_size_pt = '18'; bold = $true; text_color = '17365D'; native_picture = $true; image_pixel_size = @(16, 12); image_extent_emu = @(457200, 342900); theme_master_layout = $true
            preservation_regions = @(
                [ordered]@{ part = 'ppt/slides/slide1.xml'; anchor = '/p:sld/p:cSld/p:spTree/p:sp/p:nvSpPr/p:cNvPr[@id="2"]/a:extLst'; occurrence = 1; match_count = 1; sha256 = $pptxCnvPrHash }
                [ordered]@{ part = 'ppt/slides/slide1.xml'; anchor = '/p:sld/p:cSld/p:spTree/mc:AlternateContent'; occurrence = 1; match_count = 1; sha256 = $pptxAlternateHash }
                [ordered]@{ part = 'ppt/slides/slide1.xml'; anchor = '/p:sld/p:cSld/p:spTree/p:sp/p:spPr/@custom:flag'; occurrence = 1; match_count = 1; sha256 = Get-TextSha256 'custom:flag="keep"' }
            )
        }
    }
    [ordered]@{
        id = 'docx.basic-preserve'; path = 'office/basic-preserve.docx'; format = 'docx'; origin = 'author-created'; license = $commonSources.generated
        sha256 = Get-Sha256 (Join-Path $fixtureRoot 'office/basic-preserve.docx')
        checks = @(
            [ordered]@{ id = 'native-image-payload'; part = 'word/media/pixel.png'; sha256 = $pixelHash }
            [ordered]@{ id = 'opaque-xml-payload'; part = 'customXml/item1.xml'; sha256 = $docxOpaqueHash; contains = 'Preserve-XML-DOCX-42' }
            [ordered]@{ id = 'header-payload'; part = 'word/header1.xml'; sha256 = $docxHeaderHash }
            [ordered]@{ id = 'footer-payload'; part = 'word/footer1.xml'; sha256 = $docxFooterHash }
            [ordered]@{ id = 'document-relationships'; part = 'word/_rels/document.xml.rels'; sha256 = $docxRelsHash }
            [ordered]@{ id = 'protected-alternate-content'; part = 'word/document.xml'; anchor = '/w:document/w:body/w:p[@w14:paraId="0A000001"]/mc:AlternateContent'; occurrence = 1; match_count = 1; fragment_sha256 = $docxAlternateHash; fragment_xml = $docxAlternateRegion }
        )
        expected = [ordered]@{
            editable_text = $fixtureText.basic; paragraph_style = 'LunaTitle'; character_style = 'LunaAccent'; merged_table_grid_span = 2; inline_image = $true; image_pixel_size = @(16, 12); image_extent_emu = @(152400, 114300); page_setup = 'A4-ish twips'; custom_xml_preserve = $true
            preservation_regions = @([ordered]@{ part = 'word/document.xml'; anchor = '/w:document/w:body/w:p[@w14:paraId="0A000001"]/mc:AlternateContent'; occurrence = 1; match_count = 1; sha256 = $docxAlternateHash })
        }
    }
    [ordered]@{
        id = 'hwpx.image-and-opaque-preserve'; path = 'hwpx/opaque-image-preserve.hwpx'; format = 'hwpx'; origin = 'derived-from-public-fixture'; license = $commonSources.rhwp.license
        sha256 = Get-Sha256 $hwpxTarget
        checks = @(
            [ordered]@{ id = 'native-image-payload'; part = 'BinData/image1.bmp'; sha256 = $hwpxImageHash }
            [ordered]@{ id = 'opaque-xml-payload'; part = 'Luna/opaque.xml'; sha256 = $hwpxOpaqueHash; contains = 'Preserve-XML-HWPX-42' }
            [ordered]@{ id = 'manifest-item'; part = 'Contents/content.hpf'; sha256 = $hwpxManifestHash; contains = 'href="Luna/opaque.xml"' }
            [ordered]@{ id = 'editable-text'; part = 'Contents/section0.xml'; contains = $fixtureText.jari }
            [ordered]@{ id = 'character-style-map'; part = 'Contents/header.xml'; contains = '<hh:charPr' }
            [ordered]@{ id = 'protected-text-attribute'; part = 'Contents/section0.xml'; anchor = '/hs:sec/hp:p/hp:run/hp:t[@hx:mark="preserve-text-42"]'; occurrence = 1; match_count = 1; fragment_sha256 = $hwpxTextRegionHash; fragment_xml = $hwpxTextRegion }
        )
        expected = [ordered]@{
            section = 'Contents/section0.xml'; text = @($fixtureText.jari, $fixtureText.eoullim, $fixtureText.geulap, $fixtureText.geuldwi, $fixtureText.geulja); style_definitions = 'Contents/header.xml'; preserve_image_byte_for_byte = $true
            preservation_regions = @([ordered]@{ part = 'Contents/section0.xml'; anchor = '/hs:sec/hp:p/hp:run/hp:t[@hx:mark="preserve-text-42"]'; occurrence = 1; match_count = 1; sha256 = $hwpxTextRegionHash })
            provenance = [ordered]@{ repository = $commonSources.rhwp.repository; commit = $commonSources.rhwp.commit; path = 'samples/test-image.hwpx'; source_sha256 = Get-Sha256 $hwpxSource }
        }
    }
    [ordered]@{
        id = 'hwp.binary-source'; path = 'hwp/test-image.hwp'; format = 'hwp5'; origin = 'public-fixture-copy'; license = $commonSources.rhwp.license
        sha256 = $hwpSha
        checks = @([ordered]@{ id = 'compound-file-signature'; part = 'file-header'; bytes_hex = 'D0CF11E0A1B11AE1' })
        expected = [ordered]@{ embedded_picture = $true; do_not_treat_zip_as_hwp = $true; provenance = [ordered]@{ repository = $commonSources.rhwp.repository; commit = $commonSources.rhwp.commit; path = 'samples/test-image.hwp'; source_sha256 = Get-Sha256 (Join-Path $sourceRoot 'test-image.hwp') } }
    }
    [ordered]@{
        id = 'figure.numeric-sd'; path = 'data/known-sd.csv'; format = 'csv'; origin = 'author-created'; license = $commonSources.generated
        sha256 = Get-Sha256 (Join-Path $fixtureRoot 'data/known-sd.csv')
        checks = @([ordered]@{ id = 'original-cell-strings'; values = @('0.10', '0.20', '0.30', '0.01', '0.02', '0.03', '3', 'ratio') })
        expected = [ordered]@{ row_count = 3; mean_original = @('0.10', '0.20', '0.30'); sd_original = @('0.01', '0.02', '0.03'); n = 3; unit = 'ratio'; parsed_decimal_strings = @('0.1', '0.2', '0.3') }
    }
    [ordered]@{
        id = 'figure.ambiguous-and-invalid'; path = 'data/invalid-cases.csv'; format = 'csv'; origin = 'author-created'; license = $commonSources.generated
        sha256 = Get-Sha256 (Join-Path $fixtureRoot 'data/invalid-cases.csv')
        checks = @([ordered]@{ id = 'case-ids'; values = @('missing_value', 'ambiguous_thousands', 'negative_error', 'asymmetric_error', 'invalid_bounds', 'ci_without_level', 'missing_n', 'log_nonpositive_lower', 'duplicate_category') })
        expected = [ordered]@{ missing_value = 'keep empty; never coerce to zero; break series by default'; ambiguous_thousands = 'diagnose locale ambiguity'; negative_error = 'reject negative magnitude'; asymmetric_error = 'use explicit lower/upper bounds only'; invalid_bounds = 'diagnose lower greater than upper'; ci_without_level = 'diagnose missing confidence level'; missing_n = 'do not infer'; log_nonpositive_lower = 'diagnose negative lower bound; do not clip'; duplicate_category = 'preserve both rows and diagnose absent grouping' }
    }
    [ordered]@{
        id = 'figure.pdf-evidence-p12'; path = 'pdf/evidence-p12.pdf'; format = 'pdf'; origin = 'author-created'; license = $commonSources.generated
        sha256 = Get-Sha256 $pdfPath
        checks = @([ordered]@{ id = 'source-region-and-pagination'; physical_page_index = 11; viewer_page_number = 12; printed_page_label = '9'; crop_box_pt = @(18, 20, 594, 760); rotate = 0; text = 'Evidence: A causes B in this controlled fixture.'; source_bbox_emu_xywh = @(914400, 762000, 3302000, 254000) })
        expected = [ordered]@{
            physical_page_count = 12
            physical_page_index_base = 0
            viewer_page_number_base = 1
            emu_per_pdf_point = 12700
            figure_relation = ('A' + $fixtureText.arrow + 'B')
            evidence = [ordered]@{
                id = 'evidence-p12-ab'
                source_hash = Get-Sha256 $pdfPath
                origin = 'paper_evidence'
                physical_page_index = 11
                region = [ordered]@{ x = 914400; y = 762000; width = 3302000; height = 254000 }
                pdf_geometry = [ordered]@{
                    media_box = [ordered]@{ x_min = 0; y_min = 0; x_max = 7772400; y_max = 10058400 }
                    crop_box = [ordered]@{ x_min = 228600; y_min = 254000; x_max = 7543800; y_max = 9652000 }
                    user_unit = '1'
                    rotation_degrees = 0
                    printed_page_label = '9'
                }
                text = 'Evidence: A causes B in this controlled fixture.'
                review_state = 'unreviewed'
            }
            source_scopes = @([ordered]@{ source_hash = Get-Sha256 $pdfPath; physical_page_indices = @(11) })
        }
    }
    [ordered]@{
        id = 'figure.pdf-evidence-p12-userunit2'; path = 'pdf/evidence-p12-userunit2.pdf'; format = 'pdf'; origin = 'derived-from-author-created'; license = $commonSources.generated
        sha256 = Get-Sha256 $pdfUserUnit2Path
        checks = @([ordered]@{ id = 'user-unit-region-and-pagination'; physical_page_index = 11; viewer_page_number = 12; printed_page_label = '9'; crop_box_pt = @(18, 20, 594, 760); user_unit = '2'; rotation = 0; text = 'Evidence: A causes B in this controlled fixture.'; source_bbox_emu_xywh = @(1828800, 1524000, 6604000, 508000) })
        expected = [ordered]@{
            physical_page_count = 12
            physical_page_index_base = 0
            viewer_page_number_base = 1
            emu_per_pdf_point = 12700
            figure_relation = ('A' + $fixtureText.arrow + 'B')
            evidence = [ordered]@{
                id = 'evidence-p12-ab-userunit2'
                source_hash = Get-Sha256 $pdfUserUnit2Path
                origin = 'paper_evidence'
                physical_page_index = 11
                region = [ordered]@{ x = 1828800; y = 1524000; width = 6604000; height = 508000 }
                pdf_geometry = [ordered]@{
                    media_box = [ordered]@{ x_min = 0; y_min = 0; x_max = 15544800; y_max = 20116800 }
                    crop_box = [ordered]@{ x_min = 457200; y_min = 508000; x_max = 15087600; y_max = 19304000 }
                    user_unit = '2'
                    rotation_degrees = 0
                    printed_page_label = '9'
                }
                text = 'Evidence: A causes B in this controlled fixture.'
                review_state = 'unreviewed'
            }
            source_scopes = @([ordered]@{ source_hash = Get-Sha256 $pdfUserUnit2Path; physical_page_indices = @(11) })
        }
    }
)
$supportFiles = @(
    [ordered]@{ path = 'assets/pixel.png'; sha256 = $pixelHash; origin = 'author-created'; license = 'CC0-1.0; preservation sentinel only' }
    [ordered]@{ path = 'assets/invalid-crc.png'; sha256 = (Get-Sha256 $invalidPngPath); origin = 'author-created negative PNG with corrupted IDAT CRC'; license = 'CC0-1.0; invalid test bytes' }
    [ordered]@{ path = '_sources/rhwp/LICENSE'; sha256 = (Get-Sha256 (Join-Path $sourceRoot 'LICENSE')); origin = 'copied from pinned source repository'; license = 'MIT' }
    [ordered]@{ path = '_sources/rhwp/test-image.hwpx'; sha256 = (Get-Sha256 $hwpxSource); origin = 'copied from pinned source repository'; license = $commonSources.rhwp.license }
    [ordered]@{ path = '_sources/rhwp/test-image.hwp'; sha256 = (Get-Sha256 (Join-Path $sourceRoot 'test-image.hwp')); origin = 'copied from pinned source repository'; license = $commonSources.rhwp.license }
)
$manifest = [ordered]@{
    schemaVersion = 1
    coordinateContract = [ordered]@{
        pdfPhysicalPageIndexBase = 0
        viewerPageNumberBase = 1
        printedPageLabel = 'independent label; never used as array index'
        pdfArea = 'cropbox top-left, unrotated, x right/y down, signed i64 EMU'
        emuPerPdfPoint = 12700
        evidence = [ordered]@{
            physicalPageIndex = 11
            viewerPageNumber = 12
            printedPageLabel = '9'
            mediaBoxPdfPoints = @(0, 0, 612, 792)
            cropBoxPdfPoints = @(18, 20, 594, 760)
            rotationDegrees = 0
            text = 'Evidence: A causes B in this controlled fixture.'
            source_bbox_emu_xywh = @(914400, 762000, 3302000, 254000)
            media_box_pdf_user_space_emu_xyxy = @(0, 0, 7772400, 10058400)
            crop_box_pdf_user_space_emu_xyxy = @(228600, 254000, 7543800, 9652000)
        }
    }
    publicSources = @($commonSources.rhwp)
    fixtures = $fixtures
    supportFiles = $supportFiles
}
$manifestText = $manifest | ConvertTo-Json -Depth 20
Write-Utf8NoBom (Join-Path $fixtureRoot 'fixture-manifest.json') ($manifestText + "`n")
Write-Host "Generated $($fixtures.Count) fixture inputs and $($supportFiles.Count) support files under $fixtureRoot"
