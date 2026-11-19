<#
.SYNOPSIS
  Export an Inventor part or assembly as an lcw-bundle/1 folder for inv2sc (and Laser Cut
  Workbench): one STEP per part, sheet metal rules and flat patterns, assembly placements,
  constraints, joints and patterns.

.DESCRIPTION
  Runs outside Inventor and talks to the running Inventor over COM, as Laser Cut Workbench's
  importer does, so nothing needs installing or compiling. The source documents are never
  modified: flat patterns that don't exist yet are made inside a transaction that is aborted.

.PARAMETER OutFolder
  Where the bundle folder is written (it is created; an existing bundle there is replaced, but
  part STEPs that are newer than their .ipt are reused).

.PARAMETER Mode
  Active (default): the document in front in Inventor. File: the .iam/.ipt given by -Path.

.EXAMPLE
  powershell -STA -ExecutionPolicy Bypass -File Export-InventorBundle.ps1 -OutFolder "C:\exports\console"
#>
param(
  [Parameter(Mandatory = $true)][string]$OutFolder,
  [ValidateSet('Active', 'File')][string]$Mode = 'Active',
  [string]$Path,
  [switch]$IncludeHidden,
  [switch]$NoFlatPatterns,
  [int]$StepProtocol = 3   # 2 = AP203, 3 = AP214, 5 = AP242 (Inventor's STEP translator values)
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$ExporterVersion = 'Export-InventorBundle 0.1.0'
$Schema = 'lcw-bundle/1'
$SheetSubtype = '{9C464203-9BAE-11D3-8BAD-0060B0CE6BB4}'
$StepAddinId = '{90AF7F40-0C01-11D5-8E83-0010B541CD80}'
$DxfFormat = 'FLAT PATTERN DXF?AcadVersion=2004&OuterProfileLayer=CUT&InteriorProfilesLayer=CUT&MergeProfilesIntoPolyline=True&RebaseGeometry=True&BendUpLayer=BEND_UP&BendDownLayer=BEND_DOWN'
Add-Type -AssemblyName Microsoft.VisualBasic

$warnings = [Collections.Generic.List[string]]::new()
function Warn($m) { $warnings.Add($m); Write-Host "warning: $m" }

# ---------------------------------------------------------------- Inventor
try { $inv = [Runtime.InteropServices.Marshal]::GetActiveObject('Inventor.Application') }
catch { Write-Error 'Inventor is not running. Start Inventor, open the assembly, and try again.'; exit 21 }

# Inventor's interop assembly gives enum names; without it we fall back to numbers.
$interop = Get-ChildItem -Path (Join-Path ([IO.Path]::GetDirectoryName($inv.InstallPath)) '*') -Recurse -Filter 'Autodesk.Inventor.Interop.dll' -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $interop) { $interop = Get-ChildItem 'C:\Program Files\Autodesk\Inventor*\Bin\Public Assemblies\Autodesk.Inventor.Interop.dll' -ErrorAction SilentlyContinue | Select-Object -First 1 }
$haveEnums = $false
if ($interop) { try { Add-Type -Path $interop.FullName; $haveEnums = $true } catch { } }
function EnumName($enumType, $value) {
  if ($haveEnums) { try { return [Enum]::GetName([Type]"Inventor.$enumType", [int]$value) } catch { } }
  return [string]$value
}
function TypeOf($o) { try { return [Microsoft.VisualBasic.Information]::TypeName($o) } catch { return '' } }

if ($Mode -eq 'Active') {
  $root = $inv.ActiveDocument
  if (-not $root) { Write-Error 'No document is open in Inventor.'; exit 22 }
} else {
  if (-not $Path) { Write-Error '-Mode File needs -Path'; exit 2 }
  $full = (Resolve-Path -LiteralPath $Path).Path
  $root = $inv.Documents | Where-Object FullFileName -eq $full | Select-Object -First 1
  if (-not $root) { $root = $inv.Documents.Open($full, $false) }
}
$rootFile = $root.FullFileName
Write-Host "Exporting $rootFile"

New-Item -ItemType Directory -Force -Path $OutFolder | Out-Null
$partsDir = Join-Path $OutFolder 'parts'
$flatDir = Join-Path $OutFolder 'flat'
New-Item -ItemType Directory -Force -Path $partsDir, $flatDir | Out-Null
Remove-Item -LiteralPath (Join-Path $OutFolder 'ready') -ErrorAction SilentlyContinue

# ---------------------------------------------------------------- helpers
$cm = 10.0  # Inventor's API lengths are centimetres
function P3($p) { if ($null -eq $p) { return $null }; @([double]$p.X * $cm, [double]$p.Y * $cm, [double]$p.Z * $cm) }
function D3($v) { if ($null -eq $v) { return $null }; @([double]$v.X, [double]$v.Y, [double]$v.Z) }
function Mat($m) {
  # Inventor matrices act on column vectors with the translation in column 4, as the bundle does.
  $out = New-Object double[] 16
  for ($r = 1; $r -le 4; $r++) { for ($c = 1; $c -le 4; $c++) {
    $v = [double]$m.Cell($r, $c)
    if ($c -eq 4 -and $r -le 3) { $v *= $cm }
    $out[($r - 1) * 4 + ($c - 1)] = $v
  } }
  , $out
}
function Prop($doc, $set, $name) { try { $v = $doc.PropertySets.Item($set).Item($name).Value; if ($v) { return [string]$v } } catch { }; $null }
function Slug([string]$s) { ($s -replace '[<>:"/\\|?*\x00-\x1f]', '-').Trim() }
function DocId($doc) {
  # One id per file and model state (an assembly may place two states of one part).
  $name = $doc.FullDocumentName
  $sha = [Security.Cryptography.SHA1]::Create()
  try { ([BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($name.ToLowerInvariant()))) -replace '-', '').Substring(0, 12).ToLowerInvariant() }
  finally { $sha.Dispose() }
}
function Perp($n) { $a = if ([math]::Abs($n[0]) -lt 0.9) { @(1, 0, 0) } else { @(0, 1, 0) }; $x = @(($n[1] * $a[2] - $n[2] * $a[1]), ($n[2] * $a[0] - $n[0] * $a[2]), ($n[0] * $a[1] - $n[1] * $a[0])); $l = [math]::Sqrt($x[0] * $x[0] + $x[1] * $x[1] + $x[2] * $x[2]); @($x[0] / $l, $x[1] / $l, $x[2] / $l) }

# The outward normal of a planar face (its plane's normal, turned when the face is reversed).
function FaceNormal($f) {
  $n = D3 $f.Geometry.Normal
  try { if ($f.IsParamReversed) { $n = @(-$n[0], -$n[1], -$n[2]) } } catch { }
  $n
}

# Geometry of a constraint or joint side, in the coordinates of the assembly it belongs to.
function Entity($e) {
  $t = TypeOf $e
  $r = [ordered]@{ kind = 'other'; point = @(0, 0, 0); dir = $null; radius = $null; rim_point = $null }
  try {
    switch -Regex ($t) {
      '^Face(Proxy)?$' {
        $g = $e.Geometry
        switch ([int]$e.SurfaceType) {
          5890 { $r.kind = 'plane'; $r.point = P3 $e.PointOnFace; $r.dir = FaceNormal $e }  # planar
          5891 { $r.kind = 'cylinder'; $r.point = P3 $g.BasePoint; $r.dir = D3 $g.AxisVector; $r.radius = [double]$g.Radius * $cm
                 foreach ($ed in $e.Edges) { if ([int]$ed.GeometryType -eq 5124) { $c = $ed.Geometry; $n = D3 $c.Normal; $p = Perp $n; $cc = P3 $c.Center; $rr = [double]$c.Radius * $cm; $r.rim_point = @(($cc[0] + $p[0] * $rr), ($cc[1] + $p[1] * $rr), ($cc[2] + $p[2] * $rr)); break } } }
          5892 { $r.kind = 'cone'; $r.point = P3 $g.BasePoint; $r.dir = D3 $g.AxisVector }
          5893 { $r.kind = 'sphere'; $r.point = P3 $g.CenterPoint; $r.radius = [double]$g.Radius * $cm }
          default { $r.point = P3 $e.PointOnFace }
        }
      }
      '^Edge(Proxy)?$' {
        $g = $e.Geometry
        if ([int]$e.GeometryType -in 5124, 5125) { $r.kind = 'edge'; $r.point = P3 $g.Center; $r.dir = D3 $g.Normal; $r.radius = [double]$g.Radius * $cm; $p = Perp $r.dir; $r.rim_point = @(($r.point[0] + $p[0] * $r.radius), ($r.point[1] + $p[1] * $r.radius), ($r.point[2] + $p[2] * $r.radius)) }
        else { $r.kind = 'axis'; $r.point = P3 $e.StartVertex.Point; $d = $e.StopVertex.Point; $s = $e.StartVertex.Point; $v = @(($d.X - $s.X), ($d.Y - $s.Y), ($d.Z - $s.Z)); $l = [math]::Sqrt($v[0] * $v[0] + $v[1] * $v[1] + $v[2] * $v[2]); if ($l -gt 0) { $r.dir = @($v[0] / $l, $v[1] / $l, $v[2] / $l) } }
      }
      '^Vertex(Proxy)?$' { $r.kind = 'point'; $r.point = P3 $e.Point }
      '^WorkPlane(Proxy)?$' { $r.kind = 'plane'; $r.point = P3 $e.Plane.RootPoint; $r.dir = D3 $e.Plane.Normal }
      '^WorkAxis(Proxy)?$' { $r.kind = 'axis'; $r.point = P3 $e.Line.RootPoint; $r.dir = D3 $e.Line.Direction }
      '^WorkPoint(Proxy)?$' { $r.kind = 'point'; $r.point = P3 $e.Point }
      default { }
    }
  } catch { }
  $r
}
function OccPath($occ) {
  if ($null -eq $occ) { return @() }
  $names = @()
  try { foreach ($o in $occ.OccurrencePath) { $names += $o.Name } } catch { $names = @($occ.Name) }
  if (-not $names.Count) { $names = @($occ.Name) }
  , $names
}

# ---------------------------------------------------------------- documents
$docs = [ordered]@{}           # id -> bundle document
$docObjects = @{}              # id -> Inventor document

# Where the time goes: seconds per kind of work, and per part, written to export-timing.txt.
$clock = [Diagnostics.Stopwatch]::StartNew()
$phase = [ordered]@{ step = 0.0; sheet_metal = 0.0; mass = 0.0 }
$partTimes = [Collections.Generic.List[object]]::new()
function Timed([string]$kind, [scriptblock]$body) {
  $sw = [Diagnostics.Stopwatch]::StartNew()
  try { & $body } finally { $phase[$kind] += $sw.Elapsed.TotalSeconds }
}

function ExportStep($doc, $file) {
  $target = Join-Path $partsDir $file
  if ((Test-Path -LiteralPath $target) -and ((Get-Item -LiteralPath $target).LastWriteTimeUtc -gt [IO.File]::GetLastWriteTimeUtc($doc.FullFileName))) { return }
  $addin = $inv.ApplicationAddIns.ItemById($StepAddinId)
  if (-not $addin.Activated) { $addin.Activate() }
  $ctx = $inv.TransientObjects.CreateTranslationContext()
  $ctx.Type = 13059   # kFileBrowseIOMechanism
  $opts = $inv.TransientObjects.CreateNameValueMap()
  if ($addin.HasSaveCopyAsOptions($doc, $ctx, $opts)) {
    # The translator fills the map with its defaults, so the entry already exists: NameValueMap.Add
    # on an existing name fails with a bare E_FAIL. Set the value instead.
    $opts.Value('ApplicationProtocolType') = $StepProtocol
  }
  $medium = $inv.TransientObjects.CreateDataMedium()
  $medium.FileName = $target
  $addin.SaveCopyAs($doc, $ctx, $opts, $medium)
}

function SheetMetal($doc, $id) {
  $cd = $doc.ComponentDefinition
  $u = $doc.UnitsOfMeasure
  $sm = [ordered]@{ thickness = [double]$cd.Thickness.Value * $cm }
  try {
    $style = $cd.ActiveSheetMetalStyle
    $sm.style = [string]$style.Name
    try { $sm.bend_radius = [double]$u.GetValueFromExpression($style.BendRadius, 'cm') * $cm } catch { }
    try { $sm.gap = [double]$u.GetValueFromExpression($style.GapSize, 'cm') * $cm } catch { }
    try { $sm.relief_width = [double]$u.GetValueFromExpression($style.BendReliefWidth, 'cm') * $cm } catch { }
    try { $sm.relief_depth = [double]$u.GetValueFromExpression($style.BendReliefDepth, 'cm') * $cm } catch { }
    $rule = if ($cd.UseSheetMetalStyleUnfoldMethod) { $style.UnfoldMethod } else { $cd.UnfoldMethod }
    if ($rule) {
      $sm.unfold_method = if ([int]$rule.UnfoldMethodType -eq 28673) { 'linear' } else { (EnumName 'UnfoldMethodTypeEnum' $rule.UnfoldMethodType) }
      if ([int]$rule.UnfoldMethodType -eq 28673) { try { $sm.k_factor = [double]$u.GetValueFromExpression([string]$rule.kFactor, '') } catch { try { $sm.k_factor = [double]$rule.kFactor } catch { } } }
    }
  } catch { Warn "$($doc.DisplayName): could not read its sheet metal rule ($($_.Exception.Message))" }
  # Bends of the folded part: a point on each bend's face (what SolidCraft's convert picks).
  $points = @()
  try { foreach ($b in $cd.Bends) { try { $f = $b.FrontFaces.Item(1); $points += , (P3 $f.PointOnFace) } catch { } } } catch { }
  $sm.bend_points = $points
  # The fixed face: Inventor's flat pattern base face, else the largest planar face.
  try {
    $base = $null
    if ($cd.HasFlatPattern) { try { $base = $cd.FlatPattern.BaseFace } catch { } }
    if (-not $base) { $base = $cd.SurfaceBodies.Item(1).Faces | Where-Object SurfaceType -eq 5890 | Sort-Object { $_.Evaluator.Area } -Descending | Select-Object -First 1 }
    if ($base) {
      $sm.base_point = P3 $base.PointOnFace; $sm.base_normal = FaceNormal $base
    }
  } catch { }
  if (-not $NoFlatPatterns) {
    $txn = $null
    try {
      if (-not $cd.HasFlatPattern) {
        $txn = $inv.TransactionManager.StartTransaction($doc, 'inv2sc temporary flat pattern')
        $cd.Unfold(); $cd.FlatPattern.ExitEdit()
      }
      $fp = $cd.FlatPattern
      $bends = @()
      foreach ($b in $fp.FlatBendResults) {
        if ($b.IsOnBottomFace) { continue }
        $e = $b.Edge
        if (-not $e.StartVertex -or -not $e.StopVertex) { continue }
        $a = $e.StartVertex.Point; $z = $e.StopVertex.Point
        $bends += [ordered]@{ start = @(([double]$a.X * $cm), ([double]$a.Y * $cm)); end = @(([double]$z.X * $cm), ([double]$z.Y * $cm))
          angle_deg = [math]::Abs([double]$b.Angle) * 180 / [math]::PI; direction = $(if ($b.IsDirectionUp) { 'up' } else { 'down' }); inner_radius = [double]$b.InnerRadius * $cm }
      }
      $dxf = "$id.dxf"
      try { $cd.DataIO.WriteDataToFile($DxfFormat, (Join-Path $flatDir $dxf)) } catch { $dxf = $null; Warn "$($doc.DisplayName): flat pattern DXF failed ($($_.Exception.Message))" }
      $box = $fp.Body.RangeBox
      $sm.flat = [ordered]@{ dxf = $(if ($dxf) { "flat/$dxf" } else { $null }); bends = $bends; size = @((([double]$box.MaxPoint.X - $box.MinPoint.X) * $cm), (([double]$box.MaxPoint.Y - $box.MinPoint.Y) * $cm)) }
    } catch { Warn "$($doc.DisplayName): no flat pattern ($($_.Exception.Message))" }
    finally { if ($txn) { $txn.Abort() } }
  }
  $sm
}

# $Geometry is false for a document reached only as the source of a derived part (often a large
# master model): it is recorded by name, with no STEP and no mass, unless the assembly also places it.
function AddGeometry($doc, $d, $id) {
  $name = $d.name
  $t0 = $clock.Elapsed.TotalSeconds
  try {
    $null = Timed 'mass' {
      $mp = $doc.ComponentDefinition.MassProperties
      $box = $doc.ComponentDefinition.RangeBox
      $d.mass = [ordered]@{ volume_mm3 = [double]$mp.Volume * 1000; area_mm2 = [double]$mp.Area * 100; bbox_min = P3 $box.MinPoint; bbox_max = P3 $box.MaxPoint }
    }
  } catch { }
  if ($d.kind -ne 'part') { return }
  $file = "$id.stp"
  try { $null = Timed 'step' { ExportStep $doc $file }; $d.step = "parts/$file" } catch { Warn "$name`: STEP export failed ($($_.Exception.Message))" }
  if ($doc.SubType -ieq $SheetSubtype) {
    try { $null = Timed 'sheet_metal' { $d.sheet_metal = SheetMetal $doc $id } } catch { Warn "$name`: sheet metal data could not be read ($($_.Exception.Message))" }
  }
  $took = $clock.Elapsed.TotalSeconds - $t0
  $partTimes.Add([pscustomobject]@{ Name = $name; Seconds = $took })
  Write-Host ("  part {0} ({1:n1} s)" -f $name, $took)
  $d.has_geometry = $true
}

function AddDocument($doc, [bool]$Geometry = $true) {
  $id = DocId $doc
  if ($docs.Contains($id)) {
    if ($Geometry -and -not $docs[$id].has_geometry -and $docs[$id].kind -eq 'part') { AddGeometry $doc $docs[$id] $id }
    return $id
  }
  $isAsm = [int]$doc.DocumentType -eq 12291
  $name = [IO.Path]::GetFileNameWithoutExtension($doc.FullFileName)
  $d = [ordered]@{ id = $id; kind = $(if ($isAsm) { 'assembly' } else { 'part' }); name = $name; file = $doc.FullFileName
    part_number = Prop $doc 'Design Tracking Properties' 'Part Number'; description = Prop $doc 'Design Tracking Properties' 'Description'
    material = Prop $doc 'Design Tracking Properties' 'Material'; step = $null; mass = $null; sheet_metal = $null; derived = $null; properties = [ordered]@{} }
  foreach ($k in 'Revision Number', 'Designer', 'Project', 'Stock Number', 'Vendor') { $v = Prop $doc 'Design Tracking Properties' $k; if ($v) { $d.properties[$k] = $v } }
  foreach ($k in 'Title', 'Author', 'Comments') { $v = Prop $doc 'Inventor Summary Information' $k; if ($v) { $d.properties[$k] = $v } }
  $docs[$id] = $d
  $docObjects[$id] = $doc
  if ($isAsm) {
    AddGeometry $doc $d $id
    $d.occurrences = @(); $d.constraints = @(); $d.joints = @(); $d.patterns = @()
    ReadAssembly $doc $d
  } else {
    if ($Geometry) { AddGeometry $doc $d $id }
    try {
      foreach ($dp in $doc.ComponentDefinition.ReferenceComponents.DerivedPartComponents) {
        $src = $dp.ReferencedDocumentDescriptor.ReferencedDocument
        if ($src) { $sid = AddDocument $src $false; $mir = $false; try { $mir = [int]$dp.Definition.MirrorPlane -ne 0 } catch { }; $d.derived = [ordered]@{ source = $sid; mirrored = $mir }; break }
      }
    } catch { }
  }
  return $id
}

function ReadAssembly($doc, $d) {
  $cd = $doc.ComponentDefinition
  foreach ($occ in $cd.Occurrences) {
    if (-not $IncludeHidden -and -not $occ.Visible) { continue }
    $entry = [ordered]@{ id = $occ.Name; name = $occ.Name; document = $null; transform = (Mat $occ.Transformation)
      grounded = [bool]$occ.Grounded; suppressed = [bool]$occ.Suppressed; visible = [bool]$occ.Visible; pattern = $null; flexible = $false }
    try { $entry.flexible = [bool]$occ.Flexible } catch { }
    try { if ($occ.IsPatternElement) { $entry.pattern = $occ.PatternElement.Parent.Name } } catch { }
    if ($occ.Suppressed) { $d.occurrences += $entry; continue }
    try {
      if ($occ.ReferencedDocumentDescriptor.ReferenceMissing) { Warn "$($occ.Name): its file is missing"; continue }
      $entry.document = AddDocument $occ.Definition.Document
    } catch { Warn "$($occ.Name): $($_.Exception.Message)"; continue }
    $d.occurrences += $entry
  }
  foreach ($c in $cd.Constraints) {
    try {
      $t = TypeOf $c
      $kind = ($t -replace 'Constraint$', '').ToLowerInvariant()
      $item = [ordered]@{ id = $c.Name; name = $c.Name; kind = $kind; suppressed = [bool]$c.Suppressed; offset = $null; angle_deg = $null; solution = $null
        a = (Entity $c.EntityOne); b = (Entity $c.EntityTwo) }
      $item.a.path = OccPath $c.OccurrenceOne
      $item.b.path = OccPath $c.OccurrenceTwo
      try { $item.offset = [double]$c.Offset.Value * $cm } catch { }
      try { if ($kind -eq 'angle') { $item.angle_deg = [double]$c.Angle.Value * 180 / [math]::PI } } catch { }
      try { if ($kind -eq 'insert') { $item.solution = $(if ($c.AxesOpposed) { 'opposed' } else { 'aligned' }) } } catch { }
      try { if ($kind -eq 'angle') { $item.solution = (EnumName 'AngleConstraintSolutionTypeEnum' $c.SolutionType) } } catch { }
      $d.constraints += $item
    } catch { Warn "$($doc.DisplayName) › $($c.Name): $($_.Exception.Message)" }
  }
  try {
    foreach ($j in $cd.Joints) {
      try {
        $def = $j.Definition
        $type = (EnumName 'AssemblyJointTypeEnum' $def.JointType) -replace '^k', '' -replace 'JointType$', ''
        $side = {
          param($intent, $occ)
          $e = Entity $intent.Geometry
          $z = if ($e.dir) { $e.dir } else { @(0, 0, 1) }
          [ordered]@{ path = (OccPath $occ); origin = $e.point; z = $z; x = $null }
        }
        $item = [ordered]@{ id = $j.Name; name = $j.Name; kind = $type.ToLowerInvariant(); suppressed = [bool]$j.Suppressed; limits = @()
          a = (& $side $def.OriginOne $j.OccurrenceOne); b = (& $side $def.OriginTwo $j.OccurrenceTwo) }
        try { if ($def.HasAngularPositionLimits) { $item.limits += , @(([double]$def.AngularPositionStartLimit.Value * 180 / [math]::PI), ([double]$def.AngularPositionEndLimit.Value * 180 / [math]::PI)) } } catch { }
        try { if ($def.HasLinearPositionStartLimit -and $def.HasLinearPositionEndLimit) { $item.limits += , @(([double]$def.LinearPositionStartLimit.Value * $cm), ([double]$def.LinearPositionEndLimit.Value * $cm)) } } catch { }
        $d.joints += $item
      } catch { Warn "$($doc.DisplayName) › joint $($j.Name): $($_.Exception.Message)" }
    }
  } catch { }
  try {
    foreach ($p in $cd.OccurrencePatterns) {
      $kind = ((TypeOf $p) -replace 'OccurrencePattern$', '').ToLowerInvariant()
      $elements = @()
      foreach ($el in $p.OccurrencePatternElements) { $names = @(); foreach ($o in $el.Occurrences) { $names += $o.Name }; $elements += , $names }
      $params = [ordered]@{}
      foreach ($k in 'ColumnCount', 'RowCount', 'Count') { try { $params[$k] = [double]$p.$k.Value } catch { } }
      foreach ($k in 'ColumnOffset', 'RowOffset') { try { $params[$k] = [double]$p.$k.Value * $cm } catch { } }
      try { $params['AngleOffset'] = [double]$p.AngleOffset.Value * 180 / [math]::PI } catch { }
      $d.patterns += [ordered]@{ id = $p.Name; name = $p.Name; kind = $kind; elements = $elements; params = $params }
    }
  } catch { }
}

$rootId = AddDocument $root
foreach ($d in $docs.Values) { $d.Remove('has_geometry') }
$bundle = [ordered]@{
  schema = $Schema
  source = [ordered]@{ app = 'Autodesk Inventor'; app_version = [string]$inv.SoftwareVersion.DisplayVersion; exporter = $ExporterVersion; exported_at = (Get-Date).ToUniversalTime().ToString('o'); root_file = $rootFile }
  root = $rootId
  documents = @($docs.Values)
  warnings = @($warnings)
}
# Our own JSON writer. Windows PowerShell 5.1's ConvertTo-Json writes an array that came back from a
# function as {"value":[...],"Count":n}, which is what broke every placement matrix; it is also slow
# and has a depth limit.
$inv_culture = [Globalization.CultureInfo]::InvariantCulture
function WriteJson($sb, $v) {
  if ($null -eq $v) { [void]$sb.Append('null'); return }
  if ($v -is [string] -or $v -is [char]) {
    [void]$sb.Append('"')
    foreach ($c in ([string]$v).ToCharArray()) {
      $n = [int]$c
      if ($c -eq '"') { [void]$sb.Append('\"') } elseif ($c -eq '\') { [void]$sb.Append('\\') }
      elseif ($n -lt 32) { [void]$sb.Append(('\u{0:x4}' -f $n)) } else { [void]$sb.Append($c) }
    }
    [void]$sb.Append('"'); return
  }
  if ($v -is [bool]) { [void]$sb.Append($(if ($v) { 'true' } else { 'false' })); return }
  if ($v -is [double] -or $v -is [single]) {
    $d = [double]$v
    if ([double]::IsNaN($d) -or [double]::IsInfinity($d)) { [void]$sb.Append('null') } else { [void]$sb.Append($d.ToString('R', $inv_culture)) }
    return
  }
  if ($v -is [int] -or $v -is [long] -or $v -is [int16] -or $v -is [byte] -or $v -is [uint32] -or $v -is [uint64] -or $v -is [decimal]) { [void]$sb.Append(([IConvertible]$v).ToString($inv_culture)); return }
  if ($v -is [Collections.IDictionary]) {
    [void]$sb.Append('{'); $first = $true
    foreach ($k in @($v.Keys)) {
      if (-not $first) { [void]$sb.Append(',') }; $first = $false
      WriteJson $sb ([string]$k); [void]$sb.Append(':'); WriteJson $sb $v[$k]
    }
    [void]$sb.Append('}'); return
  }
  if ($v -is [Collections.IEnumerable]) {
    [void]$sb.Append('['); $first = $true
    foreach ($item in $v) { if (-not $first) { [void]$sb.Append(',') }; $first = $false; WriteJson $sb $item }
    [void]$sb.Append(']'); return
  }
  WriteJson $sb ([string]$v)
}
$sbJson = New-Object Text.StringBuilder
WriteJson $sbJson $bundle
$json = $sbJson.ToString()
[IO.File]::WriteAllText((Join-Path $OutFolder 'bundle.json'), $json, [Text.UTF8Encoding]::new($false))
$total = $clock.Elapsed.TotalSeconds
$other = $total - $phase.step - $phase.sheet_metal - $phase.mass
$timing = @("Export of $rootFile", ("Total {0:n1} s for {1} documents" -f $total, $docs.Count),
  ("  STEP export            {0,8:n1} s" -f $phase.step), ("  sheet metal and flats  {0,8:n1} s" -f $phase.sheet_metal),
  ("  mass and size          {0,8:n1} s" -f $phase.mass), ("  walk, constraints, joints, patterns {0,8:n1} s" -f $other), '', 'Slowest parts:')
$timing += $partTimes | Sort-Object Seconds -Descending | Select-Object -First 15 | ForEach-Object { "  {0,8:n1} s  {1}" -f $_.Seconds, $_.Name }
[IO.File]::WriteAllLines((Join-Path $OutFolder 'export-timing.txt'), [string[]]$timing)
$timing | Select-Object -First 6 | ForEach-Object { Write-Host $_ }
[IO.File]::WriteAllText((Join-Path $OutFolder 'ready'), (Get-Date).ToString('o'))
Write-Host "Bundle written: $OutFolder ($($docs.Count) documents, $($warnings.Count) warnings)"
