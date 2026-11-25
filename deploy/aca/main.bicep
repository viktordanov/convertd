// deploy/aca/main.bicep (ledger item 003). Compiled and checked against Azure in plan step 9.
// One internal container app running convertd in an existing Container Apps environment.
//
//   az deployment group what-if -g <rg> -f deploy/aca/main.bicep -p deploy/aca/main.bicepparam
//   az deployment group create  -g <rg> -f deploy/aca/main.bicep -p deploy/aca/main.bicepparam
//
// The environment (VNet, egress rules, Log Analytics) is not created here: denying egress
// on Container Apps is an environment/VNet setting, outside this app.
// API version and the properties marked (unverified) are checked by `az bicep build` and a
// what-if in step 9.

@description('Container app name.')
param name string = 'convertd'

param location string = resourceGroup().location

@description('Resource ID of an existing Container Apps environment.')
param environmentId string

@description('Workload profile name in a workload-profiles environment, e.g. Consumption. Empty for a consumption-only environment.')
param workloadProfileName string = ''

@description('Full image reference: <registry>/rs-convert:v<x.y.z>.')
param image string

@description('Registry server for a private image, e.g. <name>.azurecr.io. Empty for a public image.')
param registryServer string = ''

@description('Resource ID of a user-assigned identity with pull rights on registryServer.')
param registryIdentityId string = ''

@secure()
@description('The bearer key (API_KEY).')
param apiKey string

@secure()
@description('Previous key during rotation (API_KEY_PREVIOUS). Empty when not rotating.')
param apiKeyPrevious string = ''

@description('vCPU per replica, as a string because Bicep has no decimals.')
param cpu string = '2.0'

param memory string = '4Gi'

param minReplicas int = 1
param maxReplicas int = 5

@description('Concurrent requests per replica before a new replica is added: OFFICE_WORKERS + 1.')
param concurrentRequests int = 2

@description('Seconds in-flight jobs get after SIGTERM. Equal to TIMEOUT_MAX_SECONDS drains fully.')
param shutdownGraceSeconds int = 210

@description('Upper bound for timeout_seconds (contract: 5-210). Lower it only if step 9 measures that the 240 s platform limit cuts requests earlier.')
param timeoutMaxSeconds int = 210

@description('Disk budget for everything under TMP_DIR (lane 001). Must stay below the EmptyDir size minus 1 GiB of headroom for worker slots, profiles and logs. ACA fixes the replica storage by vCPU (unverified: 8 GiB at 2 vCPU); the volume is treated as 4 GiB.')
param tmpMaxBytes int = 3221225472

var useIdentity = !empty(registryIdentityId)
var secrets = concat(
  [ { name: 'api-key', value: apiKey } ],
  empty(apiKeyPrevious) ? [] : [ { name: 'api-key-previous', value: apiKeyPrevious } ]
)
var keyEnv = concat(
  [ { name: 'API_KEY', secretRef: 'api-key' } ],
  empty(apiKeyPrevious) ? [] : [ { name: 'API_KEY_PREVIOUS', secretRef: 'api-key-previous' } ]
)

resource app 'Microsoft.App/containerApps@2024-03-01' = {
  name: name
  location: location
  identity: useIdentity ? {
    type: 'UserAssigned'
    userAssignedIdentities: { '${registryIdentityId}': {} }
  } : { type: 'None' }
  properties: {
    environmentId: environmentId
    workloadProfileName: empty(workloadProfileName) ? null : workloadProfileName
    configuration: {
      activeRevisionsMode: 'Single'
      ingress: {
        external: false            // internal to the environment
        targetPort: 8080
        transport: 'http'
        allowInsecure: false
      }
      secrets: secrets
      registries: empty(registryServer) ? [] : [
        { server: registryServer, identity: registryIdentityId }
      ]
    }
    template: {
      // Must exceed shutdownGraceSeconds; there is no preStop hook on Container Apps (unverified).
      terminationGracePeriodSeconds: shutdownGraceSeconds + 15
      containers: [
        {
          name: 'convertd'
          image: image
          args: [ 'serve' ]
          resources: {
            cpu: json(cpu)
            memory: memory
          }
          env: concat(keyEnv, [
            { name: 'LOG_MODE', value: 'prod' }
            { name: 'TMP_DIR', value: '/tmp/convert' }
            { name: 'OFFICE_WORKERS', value: '1' }
            { name: 'RENDER_WORKERS', value: '2' }
            { name: 'TIMEOUT_MAX_SECONDS', value: string(timeoutMaxSeconds) }
            { name: 'SHUTDOWN_GRACE_SECONDS', value: string(shutdownGraceSeconds) }
            { name: 'TMP_MAX_BYTES', value: string(tmpMaxBytes) }
            { name: 'SANDBOX', value: 'off' }
          ])
          // Limits (unverified): failureThreshold <= 10, initialDelaySeconds <= 60.
          probes: [
            {
              type: 'Startup'
              httpGet: { path: '/readyz', port: 8080 }
              initialDelaySeconds: 2
              periodSeconds: 10
              timeoutSeconds: 2
              failureThreshold: 10
            }
            {
              type: 'Readiness'
              httpGet: { path: '/readyz', port: 8080 }
              periodSeconds: 5
              timeoutSeconds: 2
              failureThreshold: 2
            }
            {
              type: 'Liveness'
              httpGet: { path: '/healthz', port: 8080 }
              periodSeconds: 10
              timeoutSeconds: 3
              failureThreshold: 3
            }
          ]
          volumeMounts: [ { volumeName: 'tmp', mountPath: '/tmp' } ]
        }
      ]
      // EmptyDir lives on the replica's ephemeral storage, not memory (unverified size per vCPU).
      volumes: [ { name: 'tmp', storageType: 'EmptyDir' } ]
      scale: {
        minReplicas: minReplicas
        maxReplicas: maxReplicas
        rules: [
          {
            name: 'http-concurrency'
            http: { metadata: { concurrentRequests: string(concurrentRequests) } }
          }
        ]
      }
    }
  }
}

@description('Internal FQDN; callers use https://<fqdn>.')
output fqdn string = app.properties.configuration.ingress.fqdn
