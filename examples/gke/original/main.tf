# Hand-written HCL equivalent of ../main.kiln. The generated
# ../out/main.tf.json must plan identically to this file.

terraform {
  required_version = ">= 1.7.0"
  required_providers {
    google = {
      source  = "hashicorp/google"
      version = "~> 6.0"
    }
  }
}

provider "google" {
  project = "acme-dev"
  region  = "europe-west3"
}

resource "google_compute_network" "net__vpc" {
  name                    = "acme-dev"
  auto_create_subnetworks = false
}

resource "google_compute_subnetwork" "net__nodes" {
  name          = "acme-dev-nodes"
  region        = "europe-west3"
  network       = google_compute_network.net__vpc.id
  ip_cidr_range = "10.10.0.0/20"
}

resource "google_container_cluster" "primary__this" {
  name                     = "acme-dev"
  location                 = "europe-west3"
  network                  = google_compute_network.net__vpc.id
  subnetwork               = google_compute_subnetwork.net__nodes.id
  remove_default_node_pool = true
  initial_node_count       = 1
  deletion_protection      = false

  resource_labels = {
    env        = "dev"
    managed_by = "kiln"
  }

  workload_identity_config {
    workload_pool = "acme-dev.svc.id.goog"
  }
}

resource "google_container_node_pool" "primary__default" {
  name     = "default"
  cluster  = google_container_cluster.primary__this.name
  location = "europe-west3"

  autoscaling {
    min_node_count = 1
    max_node_count = 5
  }

  node_config {
    machine_type = "e2-standard-4"
    spot         = false
    labels = {
      env        = "dev"
      managed_by = "kiln"
      pool       = "default"
    }
  }
}

resource "google_container_node_pool" "primary__batch" {
  name     = "batch"
  cluster  = google_container_cluster.primary__this.name
  location = "europe-west3"

  autoscaling {
    min_node_count = 0
    max_node_count = 20
  }

  node_config {
    machine_type = "c3-highcpu-8"
    spot         = true
    labels = {
      env        = "dev"
      managed_by = "kiln"
      pool       = "batch"
    }
  }
}

resource "google_service_account" "billing__sa" {
  account_id   = "billing-api"
  display_name = "billing-api (2 roles)"
  project      = "acme-dev"
}

resource "google_project_iam_member" "billing__roles_pubsub_publisher" {
  project = "acme-dev"
  role    = "roles/pubsub.publisher"
  member  = "serviceAccount:${google_service_account.billing__sa.email}"
}

resource "google_project_iam_member" "billing__roles_cloudsql_client" {
  project = "acme-dev"
  role    = "roles/cloudsql.client"
  member  = "serviceAccount:${google_service_account.billing__sa.email}"
}

output "cluster_name" {
  value = google_container_cluster.primary__this.name
}

output "cluster_endpoint" {
  value     = google_container_cluster.primary__this.endpoint
  sensitive = true
}

output "billing_service_account" {
  value = google_service_account.billing__sa.email
}
