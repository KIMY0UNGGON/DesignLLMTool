$ErrorActionPreference = 'Stop'
# Windows PowerShell 5.1 notes: Add-Type compiles C# 5 only, and a stale LIB variable (old Visual Studio path)
# turns compiler warning CS1668 into an error. The script is BOM-less, so 5.1 reads it in the ANSI code page:
# every non-ASCII literal below is therefore written as a \uXXXX escape.
$env:LIB = $null
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName WindowsBase
if (-not ('LunaPngCrc' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Text;

public static class LunaPngCrc {
    private static uint ReadBe32(byte[] b, int i) {
        return ((uint)b[i] << 24) | ((uint)b[i + 1] << 16) | ((uint)b[i + 2] << 8) | b[i + 3];
    }
    private static uint Crc32(byte[] bytes) {
        uint c = 0xffffffffu;
        foreach (byte b in bytes) {
            c ^= b;
            for (int i = 0; i < 8; i++) c = (c & 1) != 0 ? (c >> 1) ^ 0xedb88320u : c >> 1;
        }
        return c ^ 0xffffffffu;
    }
    public static bool Valid(byte[] png) {
        byte[] sig = new byte[] { 137, 80, 78, 71, 13, 10, 26, 10 };
        if (png == null || png.Length < 20) return false;
        for (int i = 0; i < sig.Length; i++) if (png[i] != sig[i]) return false;
        int offset = 8;
        bool ended = false;
        while (offset <= png.Length - 12) {
            uint rawLength = ReadBe32(png, offset);
            if (rawLength > int.MaxValue) return false;
            int length = (int)rawLength;
            long next = (long)offset + 12 + length;
            if (next > png.Length) return false;
            byte[] crcInput = new byte[4 + length];
            Array.Copy(png, offset + 4, crcInput, 0, crcInput.Length);
            if (Crc32(crcInput) != ReadBe32(png, offset + 8 + length)) return false;
            string type = Encoding.ASCII.GetString(png, offset + 4, 4);
            offset = (int)next;
            if (type == "IEND") {
                ended = length == 0 && offset == png.Length;
                break;
            }
        }
        return ended;
    }
}
'@
}

$textBasic = [regex]::Unescape('\uAE30\uBCF8 \uD14D\uC2A4\uD2B8')
$textMergedCell = [regex]::Unescape('\uBCD1\uD569 \uD45C \uC140')
$textLeftCell = [regex]::Unescape('\uC67C\uCABD \uC140')
$textRightCell = [regex]::Unescape('\uC624\uB978\uCABD \uC140')
$hwpxTexts = @(
    [regex]::Unescape('\uC790\uB9AC\uCC28\uC9C0'), [regex]::Unescape('\uC5B4\uC6B8\uB9BC'), [regex]::Unescape('\uAE00\uC55E\uC73C\uB85C'),
    [regex]::Unescape('\uAE00\uB4A4\uB85C'), [regex]::Unescape('\uAE00\uC790\uCC98\uB7FC \uCDE8\uAE09')
)
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$fixtureRoot = Join-Path $repoRoot 'fixtures'
$manifest = Get-Content -Raw -Encoding UTF8 -LiteralPath (Join-Path $fixtureRoot 'fixture-manifest.json') | ConvertFrom-Json
$passed = 0
$failed = 0

function Assert-That([bool]$Condition, [string]$Name) {
    if ($Condition) { $script:passed++; Write-Host "PASS $Name" }
    else { $script:failed++; Write-Host "FAIL $Name" }
}

function Get-Sha256([string]$Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-TextSha256([string]$Text) {
    $sha = [Security.Cryptography.SHA256]::Create()
    try { ([BitConverter]::ToString($sha.ComputeHash([Text.UTF8Encoding]::new($false).GetBytes($Text)))).Replace('-', '').ToLowerInvariant() }
    finally { $sha.Dispose() }
}

function Get-StringOccurrenceCount([string]$Text, [string]$Fragment) {
    if ([string]::IsNullOrEmpty($Fragment)) { return 0 }
    $count = 0
    $offset = 0
    while (($found = $Text.IndexOf($Fragment, $offset, [StringComparison]::Ordinal)) -ge 0) {
        $count++
        $offset = $found + $Fragment.Length
    }
    return $count
}

function Get-ZipEntrySha256([string]$Path, [string]$EntryPath) {
    $archive = [IO.Compression.ZipFile]::OpenRead($Path)
    try {
        $entry = $archive.GetEntry($EntryPath)
        if (!$entry) { return $null }
        $stream = $entry.Open()
        $sha = [Security.Cryptography.SHA256]::Create()
        try { ([BitConverter]::ToString($sha.ComputeHash($stream))).Replace('-', '').ToLowerInvariant() }
        finally { $sha.Dispose(); $stream.Dispose() }
    } finally { $archive.Dispose() }
}

function Read-ZipText([string]$Path, [string]$EntryPath) {
    $archive = [IO.Compression.ZipFile]::OpenRead($Path)
    try {
        $entry = $archive.GetEntry($EntryPath)
        if (!$entry) { return $null }
        $reader = [IO.StreamReader]::new($entry.Open(), [Text.Encoding]::UTF8)
        try { $reader.ReadToEnd() } finally { $reader.Dispose() }
    } finally { $archive.Dispose() }
}

function Test-XmlParts([string]$Path) {
    $archive = [IO.Compression.ZipFile]::OpenRead($Path)
    try {
        foreach ($entry in $archive.Entries | Where-Object { $_.FullName -match '\.(xml|rels|hpf|rdf)$' }) {
            $settings = [System.Xml.XmlReaderSettings]::new()
            $settings.DtdProcessing = [System.Xml.DtdProcessing]::Prohibit
            $settings.XmlResolver = $null
            $reader = [System.Xml.XmlReader]::Create($entry.Open(), $settings)
            try { while ($reader.Read()) { } } finally { $reader.Dispose() }
        }
        return $true
    } catch { Write-Host "XML parse detail ($Path): $($_.Exception.Message)"; return $false }
    finally { $archive.Dispose() }
}

function Test-OpcPackage([string]$Path) {
    try {
        $package = [System.IO.Packaging.Package]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read)
        try {
            $parts = @($package.GetParts())
            $rootRelationships = @($package.GetRelationships())
            return ($parts.Count -gt 1 -and @($rootRelationships | Where-Object { $_.RelationshipType -eq 'http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument' }).Count -eq 1)
        } finally { $package.Close() }
    } catch { return $false }
}

function Get-XmlAnchorCount([string]$Xml, [string]$Anchor) {
    $document = [System.Xml.XmlDocument]::new()
    $document.PreserveWhitespace = $true
    $document.XmlResolver = $null
    $document.LoadXml($Xml)
    $ns = [System.Xml.XmlNamespaceManager]::new($document.NameTable)
    $ns.AddNamespace('p', 'http://schemas.openxmlformats.org/presentationml/2006/main')
    $ns.AddNamespace('a', 'http://schemas.openxmlformats.org/drawingml/2006/main')
    $ns.AddNamespace('mc', 'http://schemas.openxmlformats.org/markup-compatibility/2006')
    $ns.AddNamespace('custom', 'urn:designllm:luna-fixture')
    $ns.AddNamespace('w', 'http://schemas.openxmlformats.org/wordprocessingml/2006/main')
    $ns.AddNamespace('w14', 'http://schemas.microsoft.com/office/word/2010/wordml')
    $ns.AddNamespace('hs', 'http://www.hancom.co.kr/hwpml/2011/section')
    $ns.AddNamespace('hp', 'http://www.hancom.co.kr/hwpml/2011/paragraph')
    $ns.AddNamespace('hx', 'urn:designllm:luna-fixture')
    @($document.SelectNodes($Anchor, $ns)).Count
}

function Get-XmlDocument([string]$Xml) {
    $document = [System.Xml.XmlDocument]::new()
    $document.PreserveWhitespace = $true
    $document.XmlResolver = $null
    $document.LoadXml($Xml)
    $document
}

function Get-ChildNames($Element) {
    @($Element.ChildNodes | Where-Object { $_.NodeType -eq 'Element' } | ForEach-Object { $_.LocalName })
}

# True when every expected name occurs, in this order (schema sequence order).
function Test-ChildOrder([string[]]$Actual, [string[]]$Expected) {
    $last = -1
    foreach ($name in $Expected) {
        $index = [array]::IndexOf($Actual, $name)
        if ($index -le $last) { return $false }
        $last = $index
    }
    return $true
}

function Get-RelationshipTarget([string]$RelsXml, [string]$TypeSuffix) {
    $match = [regex]::Match($RelsXml, 'Type="[^"]*/' + [regex]::Escape($TypeSuffix) + '" Target="([^"]+)"')
    if ($match.Success) { return $match.Groups[1].Value }
    return $null
}

$fixtureKeys = @('id', 'path', 'format', 'origin', 'license', 'sha256', 'checks', 'expected')
foreach ($fixture in $manifest.fixtures) {
    $actualKeys = @($fixture.PSObject.Properties.Name | Sort-Object)
    $expectedKeys = @($fixtureKeys | Sort-Object)
    Assert-That (($actualKeys -join ',') -eq ($expectedKeys -join ',')) "$($fixture.id): manifest fields follow the agreed fixture envelope"
    $path = Join-Path $fixtureRoot $fixture.path
    Assert-That (Test-Path -LiteralPath $path) "$($fixture.id): fixture file exists"
    if (!(Test-Path -LiteralPath $path)) { continue }
    Assert-That ((Get-Sha256 $path) -eq $fixture.sha256) "$($fixture.id): fixture SHA-256 matches manifest"

    if ($fixture.format -in @('pptx', 'docx', 'hwpx')) {
        if ($fixture.format -in @('pptx', 'docx')) {
            Assert-That (Test-OpcPackage $path) "$($fixture.id): Windows OPC reader opens package and root officeDocument relationship"
        }
        $archive = [IO.Compression.ZipFile]::OpenRead($path)
        try {
            if ($fixture.format -in @('pptx', 'docx')) {
                Assert-That ($null -ne $archive.GetEntry('[Content_Types].xml')) "$($fixture.id): package content types part exists"
            } else {
                Assert-That ($null -ne $archive.GetEntry('mimetype') -and $null -ne $archive.GetEntry('Contents/content.hpf')) "$($fixture.id): HWPX package root parts exist"
            }
            Assert-That (Test-XmlParts $path) "$($fixture.id): package XML and relationships parse"
            foreach ($check in $fixture.checks) {
                if ($check.part -and $check.sha256) {
                    Assert-That ((Get-ZipEntrySha256 $path $check.part) -eq $check.sha256) "$($fixture.id): $($check.id) SHA-256 matches"
                }
                if ($check.part -and $check.contains) {
                    $text = Read-ZipText $path $check.part
                    Assert-That (($null -ne $text) -and $text.Contains([string]$check.contains)) "$($fixture.id): $($check.id) token is present"
                }
                if ($check.part -and $check.anchor -and $check.fragment_xml -and $check.fragment_sha256) {
                    $partText = Read-ZipText $path $check.part
                    $fragmentCount = Get-StringOccurrenceCount $partText ([string]$check.fragment_xml)
                    $anchorCount = Get-XmlAnchorCount $partText ([string]$check.anchor)
                    Assert-That ((Get-TextSha256 ([string]$check.fragment_xml)) -eq $check.fragment_sha256) "$($fixture.id): $($check.id) expected fragment hash is stable"
                    Assert-That ($fragmentCount -eq [int]$check.occurrence -and $anchorCount -eq [int]$check.match_count) "$($fixture.id): $($check.id) has exact bytes at one unique part/anchor/occurrence"
                }
            }
        } finally { $archive.Dispose() }
    }
}

$pptxPath = Join-Path $fixtureRoot 'office/basic-preserve.pptx'
$pptxSlide = Read-ZipText $pptxPath 'ppt/slides/slide1.xml'
$pptxRel = Read-ZipText $pptxPath 'ppt/_rels/presentation.xml.rels'
$pptxSlideRels = Read-ZipText $pptxPath 'ppt/slides/_rels/slide1.xml.rels'
Assert-That ($pptxSlide.Contains($textBasic) -and $pptxSlide.Contains('sz="1800"') -and $pptxSlide.Contains('b="1"') -and $pptxSlide.Contains('val="17365D"')) 'PPTX text remains a styled native shape'
Assert-That ($pptxRel.Contains('Target="luna/opaque.xml"')) 'PPTX unknown XML part is connected by an opaque relationship'
Assert-That ((Get-ZipEntrySha256 $pptxPath 'ppt/media/pixel.png') -eq (Get-Sha256 (Join-Path $fixtureRoot 'assets/pixel.png'))) 'PPTX image payload matches generated source bytes'
Assert-That ($pptxSlide.Contains('<p:pic>') -and $pptxSlide.Contains('r:embed="rId1"') -and $pptxSlideRels.Contains('Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/pixel.png"')) 'PPTX picture object resolves to the hashed media part'
Assert-That ($pptxSlide.Contains('custom:flag="keep"') -and $pptxSlide.Contains('<mc:AlternateContent>') -and $pptxSlide.Contains('<a16:creationId')) 'PPTX edited shape carries the foreign attribute and opaque XML subtrees'
# Package rules that PowerPoint enforces (a violation makes it offer repair): element order, id ranges, the slide's
# layout link, a notes master with its own theme, the presentation's property parts, and the root document properties.
$pptxPresentationDoc = Get-XmlDocument (Read-ZipText $pptxPath 'ppt/presentation.xml')
Assert-That (Test-ChildOrder (Get-ChildNames $pptxPresentationDoc.DocumentElement) @('sldMasterIdLst', 'notesMasterIdLst', 'sldIdLst', 'sldSz', 'notesSz', 'defaultTextStyle')) 'PPTX presentation.xml children follow CT_Presentation order (notesMasterIdLst before sldIdLst)'
$pptxMasterDoc = Get-XmlDocument (Read-ZipText $pptxPath 'ppt/slideMasters/slideMaster1.xml')
$pptxMasterIdValue = [long]$pptxPresentationDoc.SelectSingleNode('//*[local-name()="sldMasterId"]').GetAttribute('id')
$pptxLayoutIdValues = @($pptxMasterDoc.SelectNodes('//*[local-name()="sldLayoutId"]') | ForEach-Object { [long]$_.GetAttribute('id') })
Assert-That ($pptxLayoutIdValues.Count -gt 0 -and @($pptxLayoutIdValues | Where-Object { $_ -lt 2147483648 -or $_ -le $pptxMasterIdValue }).Count -eq 0) 'PPTX slide layout ids are at least 2147483648 and above the slide master id'
Assert-That ($pptxSlideRels.Contains('Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"')) 'PPTX slide links its slide layout'
$pptxMasterThemeTarget = Get-RelationshipTarget (Read-ZipText $pptxPath 'ppt/slideMasters/_rels/slideMaster1.xml.rels') 'theme'
$pptxNotesThemeTarget = Get-RelationshipTarget (Read-ZipText $pptxPath 'ppt/notesMasters/_rels/notesMaster1.xml.rels') 'theme'
Assert-That ($pptxNotesThemeTarget -and $pptxMasterThemeTarget -and $pptxNotesThemeTarget -ne $pptxMasterThemeTarget) 'PPTX notes master has a theme part of its own, not the slide master theme'
Assert-That ($pptxRel.Contains('/relationships/presProps" Target="presProps.xml"') -and $pptxRel.Contains('/relationships/viewProps" Target="viewProps.xml"') -and $pptxRel.Contains('/relationships/tableStyles" Target="tableStyles.xml"') -and $pptxRel.Contains('/relationships/theme" Target="theme/theme1.xml"')) 'PPTX presentation relates presProps, viewProps, tableStyles and its theme'
$pptxRootRels = Read-ZipText $pptxPath '_rels/.rels'
Assert-That ($pptxRootRels.Contains('/metadata/core-properties" Target="docProps/core.xml"') -and $pptxRootRels.Contains('/extended-properties" Target="docProps/app.xml"')) 'PPTX package root relates core and extended document properties'
$pptxContentTypes = Read-ZipText $pptxPath '[Content_Types].xml'
$pptxDeclaredParts = @(
    '<Override PartName="/ppt/presProps.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presProps+xml"/>'
    '<Override PartName="/ppt/viewProps.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.viewProps+xml"/>'
    '<Override PartName="/ppt/tableStyles.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.tableStyles+xml"/>'
    '<Override PartName="/ppt/theme/theme2.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>'
    '<Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>'
    '<Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/>'
)
Assert-That (@($pptxDeclaredParts | Where-Object { -not $pptxContentTypes.Contains($_) }).Count -eq 0) 'PPTX content types declare the presentation props, table styles, notes theme and document properties'

$docxPath = Join-Path $fixtureRoot 'office/basic-preserve.docx'
$docx = Read-ZipText $docxPath 'word/document.xml'
$docxStyles = Read-ZipText $docxPath 'word/styles.xml'
$docxRel = Read-ZipText $docxPath 'word/_rels/document.xml.rels'
Assert-That ($docx.Contains($textBasic) -and $docxStyles.Contains('LunaTitle') -and $docxStyles.Contains('LunaAccent')) 'DOCX text and named paragraph/character styles are present'
Assert-That ($docx.Contains('gridSpan w:val="2"') -and $docx.Contains('headerReference') -and $docx.Contains('footerReference')) 'DOCX fixture has a merged table plus header/footer references'
Assert-That ((Get-ZipEntrySha256 $docxPath 'word/media/pixel.png') -eq (Get-Sha256 (Join-Path $fixtureRoot 'assets/pixel.png'))) 'DOCX image payload matches generated source bytes'
Assert-That ($docx.Contains('r:embed="rId2"') -and $docxRel.Contains('Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/pixel.png"')) 'DOCX inline picture resolves to the hashed media part'
# Word rules: the settings part is related from the document part, run properties follow CT_RPr order, and w14:paraId
# values stay below 0x80000000 (the protected paragraph's id is the one the manifest anchors on).
Assert-That ($docxRel.Contains('Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings" Target="settings.xml"')) 'DOCX document part relates its settings part'
$docxStylesDoc = Get-XmlDocument $docxStyles
$docxAccentRunProps = $docxStylesDoc.SelectSingleNode('//*[local-name()="style"][@*[local-name()="styleId"]="LunaAccent"]/*[local-name()="rPr"]')
Assert-That ($null -ne $docxAccentRunProps -and (Test-ChildOrder (Get-ChildNames $docxAccentRunProps) @('rFonts', 'i', 'color'))) 'DOCX LunaAccent run properties follow CT_RPr order (rFonts, i, color)'
$docxParagraphIds = @((Get-XmlDocument $docx).SelectNodes('//*[@*[local-name()="paraId"]]') | ForEach-Object { [Convert]::ToInt64($_.GetAttribute('paraId', 'http://schemas.microsoft.com/office/word/2010/wordml'), 16) })
Assert-That ($docxParagraphIds.Count -gt 0 -and @($docxParagraphIds | Where-Object { $_ -ge 2147483648 }).Count -eq 0) 'DOCX w14:paraId values are below 0x80000000'
$tableDoc = [System.Xml.XmlDocument]::new()
$tableDoc.XmlResolver = $null
$tableDoc.LoadXml($docx)
$tableNs = [System.Xml.XmlNamespaceManager]::new($tableDoc.NameTable)
$tableNs.AddNamespace('w', 'http://schemas.openxmlformats.org/wordprocessingml/2006/main')
$tableRows = @($tableDoc.SelectNodes('/w:document/w:body/w:tbl/w:tr', $tableNs))
$tableCells = @($tableRows | ForEach-Object { @($_.SelectNodes('./w:tc', $tableNs)).Count })
$tableTexts = @($tableDoc.SelectNodes('/w:document/w:body/w:tbl//w:t', $tableNs) | ForEach-Object { $_.InnerText })
Assert-That (($tableCells -join ',') -eq '1,2' -and $tableTexts -contains $textMergedCell -and $tableTexts -contains $textLeftCell -and $tableTexts -contains $textRightCell) 'DOCX table row/cell topology, merge text, and both unmerged cell texts match fixture'

$hwpxSource = Join-Path $fixtureRoot '_sources/rhwp/test-image.hwpx'
$hwpxPath = Join-Path $fixtureRoot 'hwpx/opaque-image-preserve.hwpx'
$hwpxSourceArchive = [IO.Compression.ZipFile]::OpenRead($hwpxSource)
$hwpxArchive = [IO.Compression.ZipFile]::OpenRead($hwpxPath)
try {
    Assert-That ($hwpxArchive.Entries[0].FullName -eq 'mimetype') 'HWPX mimetype entry is first'
    # HWPX requires the mimetype entry stored without compression (local header method 0, name at offset 30).
    $hwpxBytes = [IO.File]::ReadAllBytes($hwpxPath)
    Assert-That ([BitConverter]::ToUInt16($hwpxBytes, 26) -eq 8 -and [Text.Encoding]::ASCII.GetString($hwpxBytes, 30, 8) -eq 'mimetype' -and [BitConverter]::ToUInt16($hwpxBytes, 8) -eq 0) 'HWPX mimetype entry is stored without compression'
    $contentHpf = Read-ZipText $hwpxPath 'Contents/content.hpf'
    Assert-That ($contentHpf.Contains('href="Luna/opaque.xml"')) 'HWPX manifest registers the unknown part'
    Assert-That ((Get-ZipEntrySha256 $hwpxSource 'BinData/image1.bmp') -eq (Get-ZipEntrySha256 $hwpxPath 'BinData/image1.bmp')) 'HWPX source image payload survives fixture derivation byte-for-byte'
    $hwpxSection = Read-ZipText $hwpxPath 'Contents/section0.xml'
    $hwpxHeader = Read-ZipText $hwpxPath 'Contents/header.xml'
    Assert-That (@($hwpxTexts | Where-Object { !$hwpxSection.Contains($_) }).Count -eq 0) 'HWPX has readable fixture text in the body section'
    Assert-That ($hwpxHeader.Contains('<hh:charPr') -and $hwpxSection.Contains('charPrIDRef=')) 'HWPX text refers to real character-style definitions'
    foreach ($entry in $hwpxSourceArchive.Entries | Where-Object { $_.FullName -ne 'Contents/content.hpf' }) {
        $derived = $hwpxArchive.GetEntry($entry.FullName)
        Assert-That ($null -ne $derived) "HWPX derived fixture retains source package part $($entry.FullName)"
    }
} finally { $hwpxSourceArchive.Dispose(); $hwpxArchive.Dispose() }

$hwpSource = Join-Path $fixtureRoot '_sources/rhwp/test-image.hwp'
$hwpPath = Join-Path $fixtureRoot 'hwp/test-image.hwp'
$hwpBytes = [IO.File]::ReadAllBytes($hwpPath)
Assert-That ([BitConverter]::ToString($hwpBytes[0..7]) -eq 'D0-CF-11-E0-A1-B1-1A-E1') 'HWP fixture has a real Compound File Binary signature'
Assert-That ((Get-Sha256 $hwpPath) -eq (Get-Sha256 $hwpSource)) 'HWP fixture is an unchanged copy of the pinned public sample'
Assert-That (Test-Path -LiteralPath (Join-Path $fixtureRoot '_sources/rhwp/LICENSE')) 'HWP/HWPX source license is included'

$pixelBytes = [IO.File]::ReadAllBytes((Join-Path $fixtureRoot 'assets/pixel.png'))
$invalidPngBytes = [IO.File]::ReadAllBytes((Join-Path $fixtureRoot 'assets/invalid-crc.png'))
Assert-That ([LunaPngCrc]::Valid($pixelBytes)) 'embedded checker PNG has valid CRC for every chunk'
Assert-That (-not [LunaPngCrc]::Valid($invalidPngBytes)) 'malformed negative PNG with bad CRC is rejected'
$pixelStream = [IO.MemoryStream]::new($pixelBytes)
try {
    $pixelImage = [Drawing.Image]::FromStream($pixelStream)
    Assert-That ($pixelImage.Width -eq 16 -and $pixelImage.Height -eq 12) 'PNG checker sentinel decodes at its declared size'
    Assert-That ($pixelImage.GetPixel(1, 1).ToArgb() -eq [Drawing.Color]::FromArgb(255, 220, 40, 40).ToArgb() -and $pixelImage.GetPixel(5, 1).ToArgb() -eq [Drawing.Color]::FromArgb(255, 40, 150, 70).ToArgb()) 'PNG checker regions carry distinct sample colors'
} finally { if ($pixelImage) { $pixelImage.Dispose() }; $pixelStream.Dispose() }

$knownCsv = Get-Content -Raw -LiteralPath (Join-Path $fixtureRoot 'data/known-sd.csv')
Assert-That ($knownCsv.Contains("baseline,0.10,0.01,3,ratio") -and $knownCsv.Contains("treated,0.20,0.02,3,ratio") -and $knownCsv.Contains("recovery,0.30,0.03,3,ratio")) 'CSV baseline has exact required decimal strings, SD strings, n=3, and unit'
$invalidCsv = Get-Content -Raw -LiteralPath (Join-Path $fixtureRoot 'data/invalid-cases.csv')
foreach ($case in @('missing_value', 'ambiguous_thousands', 'negative_error', 'asymmetric_error', 'invalid_bounds', 'ci_without_level', 'missing_n', 'log_nonpositive_lower', 'duplicate_category')) {
    Assert-That ($invalidCsv.Contains($case)) "CSV invalid fixture includes $case"
}
Assert-That ($invalidCsv.Contains('"1,234"')) 'CSV ambiguous number remains one quoted original cell'
Assert-That ($invalidCsv.Contains('Negative,0.20,-0.01')) 'CSV preserves a negative error magnitude for rejection testing'
$ciRow = @($invalidCsv -split "`r?`n" | Where-Object { $_.StartsWith('ci_without_level,') })
Assert-That ($ciRow.Count -eq 1 -and $ciRow[0] -eq 'ci_without_level,CI,0.20,,3,ratio,CI,,,') 'CSV CI case has an actually empty confidence-level field'

$pdfPath = Join-Path $fixtureRoot 'pdf/evidence-p12.pdf'
$pdf = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($pdfPath))
Assert-That ($pdf.StartsWith('%PDF-1.7') -and $pdf.Contains('/Count 12')) 'PDF header and 12-page page tree are present'
Assert-That ([regex]::Matches($pdf, '/Type /Page ').Count -eq 12) 'PDF has 12 physical page dictionaries'
Assert-That ([regex]::Matches($pdf, '/MediaBox \[0 0 612 792\]').Count -eq 12) 'PDF pages share the declared MediaBox'
Assert-That ([regex]::Matches($pdf, '/CropBox \[18 20 594 760\]').Count -eq 12) 'PDF pages share the declared asymmetric CropBox and zero rotation'
Assert-That ([regex]::Matches($pdf, '/UserUnit 1').Count -eq 12) 'PDF declares UserUnit=1 on every page'
Assert-That ($pdf.Contains('/PageLabels << /Nums [0 << /S /D /St 1 >> 11 << /S /D /St 9 >>] >>')) 'PDF catalog maps physical page 11 to the independent printed label 9'
Assert-That ($pdf.Contains('(Evidence: A causes B in this controlled fixture.) Tj')) 'PDF has the exact A-to-B evidence text on page 12'
Assert-That ($manifest.coordinateContract.evidence.physicalPageIndex -eq 11 -and $manifest.coordinateContract.evidence.viewerPageNumber -eq 12 -and $manifest.coordinateContract.evidence.printedPageLabel -eq '9') 'PDF physical index, viewer page, and printed label stay separate'
$evidenceBox = @($manifest.coordinateContract.evidence.source_bbox_emu_xywh)
$expectedBox = @(914400, 762000, 3302000, 254000)
Assert-That (($evidenceBox -join ',') -eq ($expectedBox -join ',')) 'PDF sourceBBox uses cropbox top-left unrotated EMU coordinates'
$pdfFixture = $manifest.fixtures | Where-Object { $_.id -eq 'figure.pdf-evidence-p12' }
$pdfEvidence = $pdfFixture.expected.evidence
$crop = $pdfEvidence.pdf_geometry.crop_box
$media = $pdfEvidence.pdf_geometry.media_box
Assert-That ($pdfEvidence.source_hash -eq $pdfFixture.sha256 -and $pdfEvidence.physical_page_index -eq 11) 'PDF Evidence records the source SHA-256 and zero-based page index'
Assert-That (($pdfFixture.expected.source_scopes[0].physical_page_indices -join ',') -eq '11') 'PDF source coverage records only physical page index 11'
Assert-That ($pdfEvidence.origin -eq 'paper_evidence' -and $pdfEvidence.review_state -eq 'unreviewed') 'PDF Evidence uses the canonical origin and stays unreviewed (automatic extraction is never pre-reviewed)'
Assert-That ($manifest.coordinateContract.pdfPhysicalPageIndexBase -eq 0 -and $manifest.coordinateContract.viewerPageNumberBase -eq 1) 'PDF manifest records the required physical/viewer index bases'
$mediaBoxValues = @($media.x_min, $media.y_min, $media.x_max, $media.y_max)
$cropBoxValues = @($crop.x_min, $crop.y_min, $crop.x_max, $crop.y_max)
Assert-That (($mediaBoxValues -join ',') -eq '0,0,7772400,10058400' -and ($cropBoxValues -join ',') -eq '228600,254000,7543800,9652000' -and $pdfEvidence.pdf_geometry.user_unit -eq '1') 'PDF geometry stores MediaBox/CropBox as PDF user-space EMU x-right/y-up boxes with explicit UserUnit'
$region = $pdfEvidence.region
$absoluteLeft = [long]$crop.x_min + [long]$region.x
$absoluteTop = [long]$crop.y_max - [long]$region.y
$absoluteBottom = $absoluteTop - [long]$region.height
$absoluteRight = $absoluteLeft + [long]$region.width
$absoluteBox = @($absoluteLeft, $absoluteBottom, $absoluteRight, $absoluteTop)
Assert-That (($absoluteBox -join ',') -eq '1143000,8636000,4445000,8890000') 'PDF cropbox region transforms to the expected absolute PDF user-space box'
$pdfPage12 = [regex]::Match($pdf, '(?s)26 0 obj\s*<< /Length \d+ >>\s*stream\s*(.*?)endstream').Groups[1].Value
Assert-That ($pdfPage12.Contains('BT /F1 20 Tf 63.3868057925789 Tz 0.44 Ts 1 0 0 1.081081081081081 90 684 Tm (Evidence: A causes B in this controlled fixture.) Tj ET') -and $pdfPage12.Contains('(Printed label: 9.) Tj')) 'PDF evidence text is positioned to the manifest sourceBBox and retains a separate visible label'
$startXref = [regex]::Match($pdf, 'startxref\s+(\d+)\s+%%EOF')
if ($startXref.Success) {
    $xrefOffset = [int]$startXref.Groups[1].Value
    Assert-That ($pdf.Substring($xrefOffset).StartsWith('xref')) 'PDF startxref points at the cross-reference table'
} else { Assert-That $false 'PDF has a valid startxref marker' }

$pdfUnit2Path = Join-Path $fixtureRoot 'pdf/evidence-p12-userunit2.pdf'
$pdfUnit2 = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($pdfUnit2Path))
$unit2Fixture = $manifest.fixtures | Where-Object { $_.id -eq 'figure.pdf-evidence-p12-userunit2' }
$unit2Evidence = $unit2Fixture.expected.evidence
Assert-That ([regex]::Matches($pdfUnit2, '/UserUnit 2').Count -eq 12) 'PDF non-default fixture sets UserUnit=2 on all pages'
Assert-That ($unit2Evidence.origin -eq 'paper_evidence' -and $unit2Evidence.review_state -eq 'unreviewed') 'PDF non-default Evidence uses the canonical origin and stays unreviewed'
Assert-That ($unit2Evidence.pdf_geometry.user_unit -eq '2' -and $unit2Evidence.physical_page_index -eq 11 -and $unit2Fixture.expected.source_scopes[0].physical_page_indices[0] -eq 11) 'PDF non-default evidence retains unit and zero-based page scope'
$unit2Crop = $unit2Evidence.pdf_geometry.crop_box
$unit2Region = $unit2Evidence.region
$unit2CropCoords = @($unit2Crop.x_min, $unit2Crop.y_min, $unit2Crop.x_max, $unit2Crop.y_max)
Assert-That (($unit2CropCoords -join ',') -eq '457200,508000,15087600,19304000') 'PDF UserUnit=2 CropBox is scaled into EMU before storage'
$unit2Left = [long]$unit2Crop.x_min + [long]$unit2Region.x
$unit2Top = [long]$unit2Crop.y_max - [long]$unit2Region.y
$unit2Bottom = $unit2Top - [long]$unit2Region.height
$unit2Right = $unit2Left + [long]$unit2Region.width
$unit2Absolute = @($unit2Left, $unit2Bottom, $unit2Right, $unit2Top)
Assert-That (($unit2Absolute -join ',') -eq '2286000,17272000,8890000,17780000') 'PDF UserUnit=2 region transform applies the scale exactly once'

$supportFiles = @($manifest.supportFiles)
foreach ($support in $supportFiles) {
    Assert-That ((Get-Sha256 (Join-Path $fixtureRoot $support.path)) -eq $support.sha256) "support file $($support.path): SHA-256 matches manifest"
}

Write-Host "RESULT fixture validation: pass=$passed fail=$failed"
if ($failed -gt 0) { exit 1 }
