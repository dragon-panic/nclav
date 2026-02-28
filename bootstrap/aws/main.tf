# bootstrap/aws/main.tf — Deploy nclav to AWS ECS Fargate
#
# This module provisions the nclav API server on AWS as an ECS Fargate service.
# It is a one-time setup; your enclave workloads are managed by nclav afterwards.
#
# What it creates:
#   - VPC + subnets (public, 2 AZs) + Internet Gateway + route table
#   - Security groups: ALB, ECS tasks, RDS
#   - ECR repository: nclav (hosts the nclav container image)
#   - RDS PostgreSQL: nclav-state-{suffix} (persistent state store)
#   - Secrets Manager secrets: nclav API token + DB URL
#   - IAM role: nclav-server (ECS task role with Organizations + STS + EC2 + IAM + Route53)
#   - ECS cluster + task definition + Fargate service
#   - Application Load Balancer (internet-facing, port 80 → ECS 8080)
#
# Networking design (no NAT Gateway — cost efficient):
#   ECS Fargate runs in public subnets with assign_public_ip=ENABLED.
#   Tasks reach AWS APIs directly via the Internet Gateway.
#   RDS is in the same VPC, accessible only via SG-to-SG rule.
#
# Prerequisites: see README.md.

terraform {
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.0"
    }
  }
}

provider "aws" {
  region = var.aws_region
}

data "aws_availability_zones" "available" {
  state = "available"
}

locals {
  az_a         = data.aws_availability_zones.available.names[0]
  az_b         = data.aws_availability_zones.available.names[1]
  ecr_image    = "${aws_ecr_repository.nclav.repository_url}:${var.nclav_image_tag}"
  nclav_image  = var.nclav_image != "" ? var.nclav_image : local.ecr_image
}

# ── VPC ────────────────────────────────────────────────────────────────────────

resource "aws_vpc" "nclav" {
  cidr_block           = "10.0.0.0/16"
  enable_dns_hostnames = true
  enable_dns_support   = true

  tags = { Name = "nclav-vpc" }
}

resource "aws_internet_gateway" "nclav" {
  vpc_id = aws_vpc.nclav.id
  tags   = { Name = "nclav-igw" }
}

resource "aws_subnet" "public_a" {
  vpc_id                  = aws_vpc.nclav.id
  cidr_block              = "10.0.1.0/24"
  availability_zone       = local.az_a
  map_public_ip_on_launch = true
  tags                    = { Name = "nclav-public-${local.az_a}" }
}

resource "aws_subnet" "public_b" {
  vpc_id                  = aws_vpc.nclav.id
  cidr_block              = "10.0.2.0/24"
  availability_zone       = local.az_b
  map_public_ip_on_launch = true
  tags                    = { Name = "nclav-public-${local.az_b}" }
}

resource "aws_route_table" "public" {
  vpc_id = aws_vpc.nclav.id
  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.nclav.id
  }
  tags = { Name = "nclav-public-rt" }
}

resource "aws_route_table_association" "public_a" {
  subnet_id      = aws_subnet.public_a.id
  route_table_id = aws_route_table.public.id
}

resource "aws_route_table_association" "public_b" {
  subnet_id      = aws_subnet.public_b.id
  route_table_id = aws_route_table.public.id
}

# ── Security groups ────────────────────────────────────────────────────────────

resource "aws_security_group" "alb" {
  name        = "nclav-alb-sg"
  description = "Allow inbound HTTP to ALB from anywhere"
  vpc_id      = aws_vpc.nclav.id

  ingress {
    from_port   = 80
    to_port     = 80
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = { Name = "nclav-alb-sg" }
}

resource "aws_security_group" "ecs" {
  name        = "nclav-ecs-sg"
  description = "Allow inbound from ALB to ECS task port 8080"
  vpc_id      = aws_vpc.nclav.id

  ingress {
    from_port       = 8080
    to_port         = 8080
    protocol        = "tcp"
    security_groups = [aws_security_group.alb.id]
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = { Name = "nclav-ecs-sg" }
}

resource "aws_security_group" "rds" {
  name        = "nclav-rds-sg"
  description = "Allow PostgreSQL from ECS tasks only"
  vpc_id      = aws_vpc.nclav.id

  ingress {
    from_port       = 5432
    to_port         = 5432
    protocol        = "tcp"
    security_groups = [aws_security_group.ecs.id]
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = { Name = "nclav-rds-sg" }
}

# ── ECR ────────────────────────────────────────────────────────────────────────

resource "aws_ecr_repository" "nclav" {
  name                 = "nclav"
  image_tag_mutability = "MUTABLE"

  image_scanning_configuration {
    scan_on_push = true
  }

  tags = { Name = "nclav" }
}

# ── RDS PostgreSQL ─────────────────────────────────────────────────────────────

resource "random_password" "db_password" {
  length  = 32
  special = false
}

resource "random_id" "db_suffix" {
  byte_length = 4
}

resource "aws_db_subnet_group" "nclav" {
  name       = "nclav-db-subnet-group"
  subnet_ids = [aws_subnet.public_a.id, aws_subnet.public_b.id]
  tags       = { Name = "nclav-db-subnet-group" }
}

resource "aws_db_instance" "nclav" {
  identifier             = "nclav-state-${random_id.db_suffix.hex}"
  engine                 = "postgres"
  engine_version         = "16"
  instance_class         = var.rds_instance_class
  allocated_storage      = 20
  storage_type           = "gp3"
  db_name                = "nclav"
  username               = "nclav"
  password               = random_password.db_password.result
  db_subnet_group_name   = aws_db_subnet_group.nclav.name
  vpc_security_group_ids = [aws_security_group.rds.id]
  publicly_accessible    = false
  skip_final_snapshot    = true
  deletion_protection    = false

  tags = { Name = "nclav-state" }
}

locals {
  db_url = "postgres://nclav:${random_password.db_password.result}@${aws_db_instance.nclav.endpoint}/nclav"
}

# ── Secrets Manager ────────────────────────────────────────────────────────────

resource "random_bytes" "api_token" {
  length = 32
}

resource "aws_secretsmanager_secret" "api_token" {
  name                    = "nclav-api-token"
  recovery_window_in_days = 0
  tags                    = { Name = "nclav-api-token" }
}

resource "aws_secretsmanager_secret_version" "api_token" {
  secret_id     = aws_secretsmanager_secret.api_token.id
  secret_string = random_bytes.api_token.hex
}

resource "aws_secretsmanager_secret" "db_url" {
  name                    = "nclav-db-url"
  recovery_window_in_days = 0
  tags                    = { Name = "nclav-db-url" }
}

resource "aws_secretsmanager_secret_version" "db_url" {
  secret_id     = aws_secretsmanager_secret.db_url.id
  secret_string = local.db_url
}

# ── IAM role for ECS task ──────────────────────────────────────────────────────

data "aws_caller_identity" "current" {}

resource "aws_iam_role" "nclav_server" {
  name = "nclav-server"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "ecs-tasks.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })

  tags = { Name = "nclav-server" }
}

# Permissions for nclav-server to manage enclave accounts and cross-account resources.
# NOTE: For Organizations permissions (CreateAccount, MoveAccount, etc.),
# your AWS account must be the management account of an AWS Organization.
# See iam_setup_commands output for instructions.
resource "aws_iam_role_policy" "nclav_server" {
  name = "nclav-server-policy"
  role = aws_iam_role.nclav_server.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid    = "OrganizationsManagement"
        Effect = "Allow"
        Action = [
          "organizations:CreateAccount",
          "organizations:DescribeCreateAccountStatus",
          "organizations:ListParents",
          "organizations:MoveAccount",
          "organizations:DescribeAccount",
          "organizations:CloseAccount",
          "organizations:ListAccounts",
        ]
        Resource = "*"
      },
      {
        Sid    = "STSCrossAccount"
        Effect = "Allow"
        Action = ["sts:AssumeRole"]
        Resource = "arn:aws:iam::*:role/OrganizationAccountAccessRole"
      },
      {
        Sid    = "SecretsManagerRead"
        Effect = "Allow"
        Action = ["secretsmanager:GetSecretValue"]
        Resource = [
          aws_secretsmanager_secret.api_token.arn,
          aws_secretsmanager_secret.db_url.arn,
        ]
      },
    ]
  })
}

# ECS execution role (for pulling ECR images and writing CloudWatch logs)
resource "aws_iam_role" "ecs_execution" {
  name = "nclav-ecs-execution"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "ecs-tasks.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy_attachment" "ecs_execution" {
  role       = aws_iam_role.ecs_execution.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy"
}

resource "aws_iam_role_policy" "ecs_execution_secrets" {
  name = "nclav-ecs-secrets"
  role = aws_iam_role.ecs_execution.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Action = ["secretsmanager:GetSecretValue"]
      Resource = [
        aws_secretsmanager_secret.api_token.arn,
        aws_secretsmanager_secret.db_url.arn,
      ]
    }]
  })
}

# ── CloudWatch log group ───────────────────────────────────────────────────────

resource "aws_cloudwatch_log_group" "nclav" {
  name              = "/ecs/nclav"
  retention_in_days = 30
}

# ── ECS cluster ───────────────────────────────────────────────────────────────

resource "aws_ecs_cluster" "nclav" {
  name = "nclav"

  setting {
    name  = "containerInsights"
    value = "disabled"
  }
}

# ── ECS task definition ────────────────────────────────────────────────────────

resource "aws_ecs_task_definition" "nclav" {
  family                   = "nclav-api"
  network_mode             = "awsvpc"
  requires_compatibilities = ["FARGATE"]
  cpu                      = var.task_cpu
  memory                   = var.task_memory
  task_role_arn            = aws_iam_role.nclav_server.arn
  execution_role_arn       = aws_iam_role.ecs_execution.arn

  container_definitions = jsonencode([{
    name      = "nclav"
    image     = local.nclav_image
    essential = true

    portMappings = [{
      containerPort = 8080
      protocol      = "tcp"
    }]

    command = concat(
      [
        "serve",
        "--bind", "0.0.0.0",
        "--cloud", "aws",
        "--aws-org-unit-id", var.aws_org_unit_id,
        "--aws-email-domain", var.aws_email_domain,
        "--aws-default-region", var.aws_region,
      ],
      var.aws_account_prefix != "" ? ["--aws-account-prefix", var.aws_account_prefix] : [],
      var.aws_cross_account_role != "OrganizationAccountAccessRole" ?
        ["--aws-cross-account-role", var.aws_cross_account_role] : [],
    )

    secrets = [
      {
        name      = "NCLAV_TOKEN"
        valueFrom = aws_secretsmanager_secret.api_token.arn
      },
      {
        name      = "NCLAV_POSTGRES_URL"
        valueFrom = aws_secretsmanager_secret.db_url.arn
      },
    ]

    logConfiguration = {
      logDriver = "awslogs"
      options = {
        "awslogs-group"         = aws_cloudwatch_log_group.nclav.name
        "awslogs-region"        = var.aws_region
        "awslogs-stream-prefix" = "nclav"
      }
    }
  }])
}

# ── ECS service ────────────────────────────────────────────────────────────────

resource "aws_ecs_service" "nclav" {
  name                               = "nclav-api"
  cluster                            = aws_ecs_cluster.nclav.id
  task_definition                    = aws_ecs_task_definition.nclav.arn
  desired_count                      = 1
  launch_type                        = "FARGATE"
  health_check_grace_period_seconds  = 60

  network_configuration {
    subnets          = [aws_subnet.public_a.id, aws_subnet.public_b.id]
    security_groups  = [aws_security_group.ecs.id]
    assign_public_ip = true
  }

  load_balancer {
    target_group_arn = aws_lb_target_group.nclav.arn
    container_name   = "nclav"
    container_port   = 8080
  }

  depends_on = [
    aws_lb_listener.nclav,
    aws_iam_role_policy_attachment.ecs_execution,
    aws_db_instance.nclav,
  ]
}

# ── Application Load Balancer ──────────────────────────────────────────────────

resource "aws_lb" "nclav" {
  name               = "nclav-api"
  internal           = false
  load_balancer_type = "application"
  security_groups    = [aws_security_group.alb.id]
  subnets            = [aws_subnet.public_a.id, aws_subnet.public_b.id]

  tags = { Name = "nclav-api" }
}

resource "aws_lb_target_group" "nclav" {
  name        = "nclav-api"
  port        = 8080
  protocol    = "HTTP"
  vpc_id      = aws_vpc.nclav.id
  target_type = "ip"

  health_check {
    path                = "/ready"
    interval            = 30
    timeout             = 5
    healthy_threshold   = 2
    unhealthy_threshold = 3
    matcher             = "200"
  }
}

resource "aws_lb_listener" "nclav" {
  load_balancer_arn = aws_lb.nclav.arn
  port              = 80
  protocol          = "HTTP"

  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.nclav.arn
  }
}
