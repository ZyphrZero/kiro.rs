# Stop and remove existing container if running
Write-Host "Stopping and removing existing kiro-rs container..."
docker stop kiro-rs 2>$null
docker rm kiro-rs 2>$null

# Start the new container
Write-Host "Starting new kiro-rs container..."
docker run -d `
  --name kiro-rs `
  -p 8990:8990 `
  -v c:\Users\User\kiro\data:/app/config/ `
  --restart unless-stopped `
  zyphrzero/kiro-rs:latest

Write-Host "Kiro.rs container started successfully!"
Write-Host "API Endpoint: http://localhost:8990/v1/messages"
Write-Host "Admin Web UI: http://localhost:8990/admin"
