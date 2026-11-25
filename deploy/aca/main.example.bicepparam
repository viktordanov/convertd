// deploy/aca/main.example.bicepparam. Copy it outside the repository, fill it in,
// and pass the key from the environment so it never lands in a file:
//   CONVERT_API_KEY=... az deployment group create -g <rg> -f deploy/aca/main.bicep -p my.bicepparam
using './main.bicep'

param environmentId = '/subscriptions/<sub>/resourceGroups/<rg>/providers/Microsoft.App/managedEnvironments/<env>'
param image = 'registry.example.com/rs-convert:v0.1.0'
param apiKey = readEnvironmentVariable('CONVERT_API_KEY')
