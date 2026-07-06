output "instance_name" {
  value = google_compute_instance.tracegate.name
}

output "zone" {
  value = google_compute_instance.tracegate.zone
}

output "external_ip" {
  value = google_compute_instance.tracegate.network_interface[0].access_config[0].nat_ip
}
