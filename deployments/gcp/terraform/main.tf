resource "google_service_account" "tracegate_vm" {
  account_id   = "tracegate-vm"
  display_name = "TraceGate v0.1 VM service account"
}

resource "google_compute_firewall" "tracegate_http" {
  name    = "tracegate-v0-1-http"
  network = "default"

  allow {
    protocol = "tcp"
    ports    = ["8080"]
  }

  source_ranges = ["0.0.0.0/0"]
  target_tags   = ["tracegate-v0-1"]
}

resource "google_compute_firewall" "tracegate_ssh" {
  name    = "tracegate-v0-1-ssh"
  network = "default"

  allow {
    protocol = "tcp"
    ports    = ["22"]
  }

  source_ranges = [var.ssh_source_cidr]
  target_tags   = ["tracegate-v0-1"]
}

resource "google_compute_instance" "tracegate" {
  name                      = "tracegate-vm"
  machine_type              = var.machine_type
  zone                      = var.zone
  allow_stopping_for_update = true
  tags                      = ["tracegate-v0-1"]

  boot_disk {
    initialize_params {
      image = "debian-cloud/debian-12"
      size  = var.disk_size_gb
      type  = "pd-standard"
    }
  }

  network_interface {
    network = "default"

    access_config {}
  }

  service_account {
    email  = google_service_account.tracegate_vm.email
    scopes = ["https://www.googleapis.com/auth/logging.write", "https://www.googleapis.com/auth/monitoring.write"]
  }

  metadata_startup_script = <<-SCRIPT
    #!/usr/bin/env bash
    set -euxo pipefail

    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    apt-get install -y --no-install-recommends docker.io docker-compose ca-certificates curl openssl
    systemctl enable --now docker
    mkdir -p /opt/tracegate

    if command -v docker-compose >/dev/null 2>&1; then
      ln -sf "$(command -v docker-compose)" /usr/local/bin/docker-compose
    else
      cat >/usr/local/bin/docker-compose <<'EOF'
#!/usr/bin/env bash
exec docker compose "$@"
EOF
      chmod +x /usr/local/bin/docker-compose
    fi
  SCRIPT
}
