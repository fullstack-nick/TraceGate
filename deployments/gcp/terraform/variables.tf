variable "project_id" {
  type        = string
  description = "Dedicated TraceGate GCP project id."

  validation {
    condition     = can(regex("^tracegate-[a-z0-9-]+$", var.project_id))
    error_message = "project_id must be a dedicated TraceGate project id beginning with tracegate-."
  }
}

variable "region" {
  type        = string
  description = "GCP region."
  default     = "us-central1"

  validation {
    condition     = contains(["us-central1", "us-east1", "us-west1"], var.region)
    error_message = "region must be one of the Compute Engine free-tier regions."
  }
}

variable "zone" {
  type        = string
  description = "GCP zone."
  default     = "us-central1-a"
}

variable "machine_type" {
  type        = string
  description = "VM machine type."
  default     = "e2-micro"

  validation {
    condition     = var.machine_type == "e2-micro"
    error_message = "v0.1 is locked to e2-micro."
  }
}

variable "disk_size_gb" {
  type        = number
  description = "Boot disk size in GB."
  default     = 30

  validation {
    condition     = var.disk_size_gb == 30
    error_message = "v0.1 is locked to a 30 GB standard persistent disk."
  }
}

variable "ssh_source_cidr" {
  type        = string
  description = "Operator public IP CIDR allowed to SSH."
}
