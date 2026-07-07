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
    condition     = contains(["e2-micro", "n2-standard-16"], var.machine_type)
    error_message = "machine_type must be e2-micro for steady state or n2-standard-16 for v1 release-quality mode."
  }
}

variable "release_quality_mode" {
  type        = bool
  description = "Whether this apply is intentionally using temporary v1 release-quality infrastructure."
  default     = false
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

variable "load_generator_enabled" {
  type        = bool
  description = "Create the temporary v1 release-quality load generator VM."
  default     = false
}

variable "load_generator_name" {
  type        = string
  description = "Temporary v1 release-quality load generator VM name."
  default     = "tracegate-v1-loadgen"
}

variable "load_generator_machine_type" {
  type        = string
  description = "Temporary v1 release-quality load generator VM machine type."
  default     = "n2-standard-8"

  validation {
    condition     = var.load_generator_machine_type == "n2-standard-8"
    error_message = "v1 load generator is locked to n2-standard-8."
  }
}

variable "load_generator_disk_size_gb" {
  type        = number
  description = "Temporary v1 release-quality load generator boot disk size in GB."
  default     = 30

  validation {
    condition     = var.load_generator_disk_size_gb == 30
    error_message = "v1 load generator is locked to a 30 GB standard persistent boot disk."
  }
}
