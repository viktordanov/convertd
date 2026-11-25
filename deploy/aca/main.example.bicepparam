// deploy/aca/main.example.bicepparam: an example parameter file for main.bicep.
// Copy it outside the repository and fill in environmentId and image. The `using` path is
// relative to the parameter file. In the copy, change it to the path from the copy to
// deploy/aca/main.bicep, for example '../rs-convert/deploy/aca/main.bicep', and pass that
// same file to -f. The key comes from the environment, so it never lands in a file:
//   export CONVERT_API_KEY='<key>'
//   az deployment group create -g <rg> -f <repo>/deploy/aca/main.bicep -p my.bicepparam
using './main.bicep'

param environmentId = '/subscriptions/<sub>/resourceGroups/<rg>/providers/Microsoft.App/managedEnvironments/<env>'
param image = 'registry.example.com/rs-convert:v0.1.0'
param apiKey = readEnvironmentVariable('CONVERT_API_KEY')
