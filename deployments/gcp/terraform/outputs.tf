output "instance_name" {
  value = google_compute_instance.tracegate.name
}

output "zone" {
  value = google_compute_instance.tracegate.zone
}

output "external_ip" {
  value = google_compute_instance.tracegate.network_interface[0].access_config[0].nat_ip
}

output "machine_type" {
  value = google_compute_instance.tracegate.machine_type
}

output "release_quality_mode" {
  value = var.release_quality_mode
}

output "load_generator_name" {
  value = try(google_compute_instance.load_generator[0].name, "")
}

output "load_generator_external_ip" {
  value = try(google_compute_instance.load_generator[0].network_interface[0].access_config[0].nat_ip, "")
}
