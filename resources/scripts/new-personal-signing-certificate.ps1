param(
  [string]$Subject = 'CN=GlazeWM Personal UIAccess',

  [Parameter(Mandatory = $true)]
  [string]$PublicCertificatePath
)

$ErrorActionPreference = 'Stop'

$resolvedOutput = [IO.Path]::GetFullPath($PublicCertificatePath)
$outputDirectory = [IO.Path]::GetDirectoryName($resolvedOutput)
New-Item -ItemType Directory -Path $outputDirectory -Force | Out-Null

$certificate = New-SelfSignedCertificate `
  -Type CodeSigningCert `
  -Subject $Subject `
  -CertStoreLocation 'Cert:\CurrentUser\My' `
  -KeyAlgorithm RSA `
  -KeyLength 3072 `
  -HashAlgorithm SHA256 `
  -KeyExportPolicy NonExportable `
  -NotAfter (Get-Date).AddYears(5)

Export-Certificate -Cert $certificate -FilePath $resolvedOutput -Force |
  Out-Null

Write-Output "thumbprint=$($certificate.Thumbprint)"
Write-Output "certificate=$resolvedOutput"
Write-Warning 'The certificate was created in CurrentUser\My but was not added to Trusted Root or Trusted Publishers.'
